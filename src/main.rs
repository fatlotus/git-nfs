use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use sha1::{Digest, Sha1};
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

use git_nfs::git::GitEngine;
use git_nfs::mount::NfsMounter;
use git_nfs::nfs::fs::GitNfsFileSystem;
use git_nfs::nfs::start_nfs_server;
use git_nfs::staging::StagingStore;
use git_nfs::vfs::inode::VfsManager;
use git_nfs::wal::recovery::RecoveredPackResult;
use git_nfs::wal::{GcsRapidWalBackend, LocalWalBackend, WalBackend, WalManager};

#[derive(Parser, Debug)]
#[command(name = "git-nfs")]
#[command(about = "NFS proxy server for browsing and modifying Git repositories on macOS without full cloning")]
struct Args {
    /// Remote Git repository URL (e.g. https://github.com/torvalds/linux.git or gs://git-on-gcs-rapid/linux/)
    #[arg(default_value = "https://github.com/torvalds/linux.git")]
    repo_url: String,

    /// Optional explicit GCS bucket for Git repository (e.g. git-on-gcs-rapid)
    #[arg(long)]
    repo_bucket: Option<String>,

    /// Optional explicit object prefix for Git repository on GCS (e.g. linux/)
    #[arg(long)]
    repo_prefix: Option<String>,

    /// Local mountpoint path
    #[arg(short, long, default_value = "/tmp/git-nfs-mount")]
    mountpoint: PathBuf,

    /// Branch or tag name to browse (defaults to HEAD / default branch)
    #[arg(short, long)]
    branch: Option<String>,

    /// Local TCP port to bind the NFS server (default: 20490, or 0 for auto-assign)
    #[arg(short, long, default_value_t = 20490)]
    port: u16,

    /// Custom blob cache and staging directory
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Optional path for output Git packfile when changes are made
    #[arg(long)]
    output_pack: Option<PathBuf>,

    /// Author signature (e.g. "User Name <user@example.com>")
    #[arg(long)]
    author: Option<String>,

    /// Run the server only without executing mount_nfs
    #[arg(long)]
    no_mount: bool,

    /// Optional GCS Rapid Bucket for appendable Write-Ahead Logging (e.g. projects/_/buckets/my-rapid-bucket)
    #[arg(long)]
    wal_bucket: Option<String>,

    /// Optional prefix for WAL objects in GCS Rapid Bucket (e.g. wal/ or repo-1/wal/)
    #[arg(long, default_value = "")]
    wal_prefix: String,
}

fn resolve_author() -> String {
    let name = Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("USER").unwrap_or_else(|_| "git-nfs".to_string())
        });

    let email = Command::new("git")
        .args(["config", "user.email"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "git-nfs@local".to_string());

    format!("{name} <{email}>")
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Determine repository target (GCS Rapid bucket or remote HTTP)
    let (is_gcs_repo, gcs_bucket, gcs_prefix) = if let Some(ref bucket) = args.repo_bucket {
        let prefix = args.repo_prefix.clone().unwrap_or_default();
        (true, bucket.clone(), prefix)
    } else if args.repo_url.starts_with("gs://") {
        let without_scheme = &args.repo_url["gs://".len()..];
        let (bucket, prefix) = match without_scheme.split_once('/') {
            Some((b, p)) => (b.to_string(), p.to_string()),
            None => (without_scheme.to_string(), String::new()),
        };
        (true, bucket, prefix)
    } else {
        (false, String::new(), String::new())
    };

    info!("=== Git NFS Proxy Server (Read-Write) ===");
    if is_gcs_repo {
        info!("Repository Type: GCS Rapid Bucket");
        info!("GCS Bucket     : {}", gcs_bucket);
        info!("GCS Prefix     : {}", gcs_prefix);
    } else {
        info!("Repository URL : {}", args.repo_url);
    }
    info!("Mountpoint     : {}", args.mountpoint.display());
    info!("Branch / Ref   : {:?}", args.branch.as_deref().unwrap_or("HEAD"));

    // Determine cache and staging directory
    let base_cache_dir = if let Some(ref dir) = args.cache_dir {
        dir.clone()
    } else {
        let mut hasher = Sha1::new();
        if is_gcs_repo {
            hasher.update(format!("gs://{gcs_bucket}/{gcs_prefix}").as_bytes());
        } else {
            hasher.update(args.repo_url.as_bytes());
        }
        let repo_slug = hex::encode(hasher.finalize());
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".cache").join("git-nfs").join(repo_slug)
    };

    // 1. Initialize Staging Store
    let staging = Arc::new(StagingStore::new(&base_cache_dir).context("Initializing Staging Store")?);

    // 2. Initialize Git Engine
    let git_engine = if is_gcs_repo {
        info!("Initializing GCS Git Engine...");
        Arc::new(
            GitEngine::new_gcs(&gcs_bucket, &gcs_prefix, Some(&base_cache_dir))
                .await
                .context("Initializing GCS Git Engine")?,
        )
    } else {
        Arc::new(
            GitEngine::new(&args.repo_url, Some(&base_cache_dir))
                .context("Initializing Git Engine")?,
        )
    };

    // 3. Fetch repository commit and tree hierarchy
    info!("Connecting to Git repository...");
    let root_oid = git_engine
        .initialize(args.branch.as_deref())
        .await
        .context("Failed to initialize Git tree hierarchy")?;

    let base_commit_oid = git_engine
        .base_commit_oid()
        .ok_or_else(|| anyhow::anyhow!("Base commit OID unavailable"))?;

    let author = args.author.unwrap_or_else(resolve_author);

    // Default WAL bucket to repository bucket if targeting GCS
    let effective_wal_bucket = args.wal_bucket.or_else(|| {
        if is_gcs_repo {
            Some(gcs_bucket.clone())
        } else {
            None
        }
    });

    let effective_wal_prefix = if args.wal_prefix.is_empty() && is_gcs_repo {
        let clean = gcs_prefix.trim_start_matches('/');
        if clean.is_empty() || clean.ends_with('/') {
            format!("{clean}wal/")
        } else {
            format!("{clean}/wal/")
        }
    } else {
        args.wal_prefix
    };

    // 4. Initialize WAL Backend (GCS Rapid Bucket or Local Filesystem)
    let wal_backend: Arc<dyn WalBackend> = if let Some(ref bucket) = effective_wal_bucket {
        info!(
            "Using GCS Rapid Bucket for WAL: {} (prefix: {:?})",
            bucket, effective_wal_prefix
        );
        Arc::new(
            GcsRapidWalBackend::new(bucket, &effective_wal_prefix)
                .await
                .context("Connecting to GCS Rapid Bucket for WAL")?,
        )
    } else {
        let local_wal_dir = base_cache_dir.join("wal");
        info!("Using local filesystem for WAL: {}", local_wal_dir.display());
        Arc::new(LocalWalBackend::new(&local_wal_dir).context("Initializing local WAL backend")?)
    };

    let mut current_root_oid = root_oid.clone();
    let mut current_base_commit_oid = base_commit_oid.clone();

    // 5. Crash Recovery Phase: Finalize prior WALs (locking out zombies) and replay uncommitted writes
    info!("Checking for uncommitted WALs and locking out zombie NFS servers...");
    if let Some(recovered) = WalManager::recover_and_replay_uncommitted(
        &wal_backend,
        &git_engine,
        &current_root_oid,
        &current_base_commit_oid,
        &author,
        "",
        &base_cache_dir,
        args.branch.as_deref(),
    )
    .await
    .context("Performing crash recovery on uncommitted WALs")?
    {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let pack_path = args.output_pack.clone().unwrap_or_else(|| {
            cwd.join(format!("pack-{}.pack", recovered.pack_sha))
        });
        let idx_path = pack_path.with_extension("idx");

        std::fs::write(&pack_path, &recovered.pack_data)
            .with_context(|| format!("Writing recovered packfile to {}", pack_path.display()))?;
        std::fs::write(&idx_path, &recovered.idx_data)
            .with_context(|| format!("Writing recovered idx file to {}", idx_path.display()))?;

        println!();
        println!("===============================================================");
        println!("🛠️  Crash Recovery: Successfully recovered uncommitted WAL writes!");
        println!("===============================================================");
        println!("📌 Recovered Commit SHA : {}", recovered.commit_oid);
        println!("📌 Base Commit SHA      : {current_base_commit_oid}");
        println!("📦 Recovered Packfile   : {}", pack_path.display());
        println!("📄 Recovered Index file : {}", idx_path.display());
        println!("📊 Recovered Objects    : {}", recovered.objects.len());
        println!("===============================================================");
        println!();

        // Apply recovered state to GitEngine so active VFS starts with the recovered files!
        git_engine.apply_recovered_objects(
            &recovered.commit_oid,
            &recovered.root_tree_oid,
            &recovered.objects,
        );
        current_root_oid = recovered.root_tree_oid.clone();
        current_base_commit_oid = recovered.commit_oid.clone();
    }

    // 6. Allocate sequential WAL sequence and start active WAL session
    let all_seqs = wal_backend.list_log_sequences().await?;
    let active_seq = all_seqs.last().map(|s| s + 1).unwrap_or(0);
    info!("Starting active WAL session with sequence {active_seq}.log");

    let repo_identifier = if is_gcs_repo {
        format!("gs://{gcs_bucket}/{gcs_prefix}")
    } else {
        args.repo_url.clone()
    };

    let wal_manager = Arc::new(WalManager::new(wal_backend.clone(), active_seq));
    wal_manager
        .start_active_wal(
            &repo_identifier,
            &current_base_commit_oid,
            &author,
            "",
        )
        .await
        .context("Initializing active WAL")?;

    // 7. Initialize VFS Manager
    let vfs = Arc::new(VfsManager::new(git_engine.clone(), &current_root_oid));

    // 8. Create Read-Write NFS File System with WAL
    let nfs_fs = GitNfsFileSystem::new(
        vfs.clone(),
        git_engine.clone(),
        staging.clone(),
        Some(wal_manager.clone()),
    );

    let commit_lock = nfs_fs.commit_lock();
    let last_mutation_time = nfs_fs.last_mutation_time();
    let has_uncommitted_changes = nfs_fs.has_uncommitted_changes();

    // 9. Start NFS Server
    let server_info = start_nfs_server("127.0.0.1", args.port, nfs_fs)
        .await
        .context("Starting NFS server")?;

    info!(
        "NFS server is active on {}:{}",
        server_info.ip, server_info.port
    );

    // 10. Mount filesystem
    let mounter = if !args.no_mount {
        Some(
            NfsMounter::mount(&server_info.ip, server_info.port, &args.mountpoint)
                .context("Mounting NFS share")?,
        )
    } else {
        info!("Skipping mount as --no-mount was passed.");
        None
    };

    // Shared OIDs protected by mutex for auto-commit background task and main thread
    let shared_root_oid = Arc::new(tokio::sync::Mutex::new(current_root_oid.clone()));
    let shared_base_commit_oid = Arc::new(tokio::sync::Mutex::new(current_base_commit_oid.clone()));

    let stop_signal = Arc::new(tokio::sync::Notify::new());
    let stop_signal_clone = stop_signal.clone();

    let auto_wal_mgr = wal_manager.clone();
    let auto_backend = wal_backend.clone();
    let auto_engine = git_engine.clone();
    let auto_staging = staging.clone();
    let auto_lock = commit_lock.clone();
    let auto_last_mut = last_mutation_time.clone();
    let auto_has_uncommitted = has_uncommitted_changes.clone();
    let auto_author = author.clone();
    let auto_repo_id = repo_identifier.clone();
    let auto_cache_dir = base_cache_dir.clone();
    let auto_branch = args.branch.clone();
    let auto_output_pack = args.output_pack.clone();
    let auto_root_oid = shared_root_oid.clone();
    let auto_base_oid = shared_base_commit_oid.clone();

    // Background task: automatically commits after 30 seconds of write inactivity
    let auto_commit_handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = stop_signal_clone.notified() => {
                    break;
                }
                _ = ticker.tick() => {
                    if !auto_has_uncommitted.load(std::sync::atomic::Ordering::Relaxed) {
                        continue;
                    }
                    let last = auto_last_mut.load(std::sync::atomic::Ordering::Relaxed);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();

                    if now >= last + 30 {
                        let _guard = auto_lock.write().await;
                        if auto_has_uncommitted.load(std::sync::atomic::Ordering::Relaxed) {
                            let mut root = auto_root_oid.lock().await;
                            let mut base = auto_base_oid.lock().await;

                            match perform_commit(
                                &auto_wal_mgr,
                                &auto_backend,
                                &auto_engine,
                                &auto_staging,
                                &mut *root,
                                &mut *base,
                                &auto_author,
                                &auto_repo_id,
                                &auto_cache_dir,
                                auto_branch.as_deref(),
                                auto_output_pack.as_ref(),
                                false,
                            ).await {
                                Ok(Some(res)) => {
                                    auto_has_uncommitted.store(false, std::sync::atomic::Ordering::Relaxed);
                                    println!();
                                    println!("===============================================================");
                                    println!("🎉 Automatically committed changes (30s inactivity):");
                                    println!("📌 New Commit SHA : {}", res.commit_oid);
                                    println!("📦 Packfile       : pack-{}.pack", res.pack_sha);
                                    println!("📊 Total objects  : {}", res.objects.len());
                                    if auto_engine.gcs_storage().is_some() {
                                        println!("☁️  GCS Storage   : Uploaded pack-{}.pack & .idx, updated refs on GCS", res.pack_sha);
                                    }
                                    println!("===============================================================");
                                    println!();
                                }
                                Ok(None) => {
                                    auto_has_uncommitted.store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                                Err(e) => {
                                    tracing::error!("Auto-commit error: {e:#}");
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    println!();
    println!("🎉 Git repository successfully mounted at: {}", args.mountpoint.display());
    println!("👉 You can now browse & EDIT it with Finder or Terminal: open {}", args.mountpoint.display());
    println!("👉 Changes are automatically committed after 30 seconds of inactivity or upon Ctrl+C.");
    println!();

    // Wait for SIGINT (Ctrl+C) or SIGTERM (kill)
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
    }
    info!("Shutting down Git NFS server...");

    // Stop background auto-commit task
    stop_signal.notify_one();
    let _ = auto_commit_handle.await;

    // Cleanly unmount first so no more writes arrive
    if let Some(m) = mounter {
        m.unmount();
    }

    // Final shutdown commit
    let _guard = commit_lock.write().await;
    let mut root_guard = shared_root_oid.lock().await;
    let mut base_guard = shared_base_commit_oid.lock().await;

    println!();
    info!("Checking for final uncommitted changes...");

    let recovered = perform_commit(
        &wal_manager,
        &wal_backend,
        &git_engine,
        &staging,
        &mut *root_guard,
        &mut *base_guard,
        &author,
        &repo_identifier,
        &base_cache_dir,
        args.branch.as_deref(),
        args.output_pack.as_ref(),
        true,
    )
    .await
    .context("Performing shutdown commit")?;

    if let Some(res) = recovered {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let pack_path = args
            .output_pack
            .unwrap_or_else(|| cwd.join(format!("pack-{}.pack", res.pack_sha)));
        let idx_path = pack_path.with_extension("idx");

        println!();
        println!("===============================================================");
        println!("🎉 Successfully generated and published Git commit from WAL!");
        println!("===============================================================");
        println!("📌 New Commit SHA : {}", res.commit_oid);
        println!("📌 Base Commit SHA: {}", *base_guard);
        println!("📦 Packfile       : {}", pack_path.display());
        println!("📄 Index file     : {}", idx_path.display());
        println!("📊 Total objects  : {}", res.objects.len());
        if git_engine.gcs_storage().is_some() {
            println!("☁️  GCS Storage   : Uploaded pack-{}.pack & .idx, updated refs on GCS", res.pack_sha);
        }
        println!();
        println!("To inspect your packfile:");
        println!("  git verify-pack -v {}", pack_path.display());
        println!("===============================================================");
    } else {
        info!("No changes were made. Clean exit.");
    }

    Ok(())
}

async fn perform_commit(
    wal_manager: &Arc<WalManager>,
    wal_backend: &Arc<dyn WalBackend>,
    git_engine: &Arc<GitEngine>,
    staging: &Arc<StagingStore>,
    current_root_oid: &mut String,
    current_base_commit_oid: &mut String,
    author: &str,
    repo_identifier: &str,
    base_cache_dir: &std::path::Path,
    branch: Option<&str>,
    output_pack: Option<&PathBuf>,
    is_shutdown: bool,
) -> Result<Option<RecoveredPackResult>> {
    wal_manager
        .finalize_active_wal()
        .await
        .context("Finalizing active WAL")?;

    let recovered = WalManager::recover_and_replay_uncommitted(
        wal_backend,
        git_engine,
        current_root_oid,
        current_base_commit_oid,
        author,
        "",
        base_cache_dir,
        branch,
    )
    .await
    .context("Replaying uncommitted WAL to build commit")?;

    if let Some(ref res) = recovered {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let pack_path = output_pack
            .cloned()
            .unwrap_or_else(|| cwd.join(format!("pack-{}.pack", res.pack_sha)));
        let idx_path = pack_path.with_extension("idx");

        std::fs::write(&pack_path, &res.pack_data)
            .with_context(|| format!("Writing packfile to {}", pack_path.display()))?;
        std::fs::write(&idx_path, &res.idx_data)
            .with_context(|| format!("Writing index file to {}", idx_path.display()))?;

        git_engine.apply_recovered_objects(
            &res.commit_oid,
            &res.root_tree_oid,
            &res.objects,
        );
        *current_root_oid = res.root_tree_oid.clone();
        *current_base_commit_oid = res.commit_oid.clone();

        staging.reset().context("Resetting staging store after commit")?;

        if !is_shutdown {
            wal_manager.advance_seq();
            wal_manager
                .start_active_wal(
                    repo_identifier,
                    current_base_commit_oid,
                    author,
                    "",
                )
                .await
                .context("Initializing next active WAL session")?;
        }
    } else if !is_shutdown {
        wal_manager.advance_seq();
        wal_manager
            .start_active_wal(
                repo_identifier,
                current_base_commit_oid,
                author,
                "",
            )
            .await
            .context("Re-initializing active WAL session")?;
    }

    Ok(recovered)
}
