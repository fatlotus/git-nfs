use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use sha1::{Digest, Sha1};
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

use git_nfs::git::builder::rebuild_git_objects;
use git_nfs::git::pack_writer::PackWriter;
use git_nfs::git::GitEngine;
use git_nfs::mount::NfsMounter;
use git_nfs::nfs::fs::GitNfsFileSystem;
use git_nfs::nfs::start_nfs_server;
use git_nfs::staging::StagingStore;
use git_nfs::vfs::inode::VfsManager;

#[derive(Parser, Debug)]
#[command(name = "git-nfs")]
#[command(about = "NFS proxy server for browsing and modifying Git repositories on macOS without full cloning")]
struct Args {
    /// Remote Git repository URL (e.g. https://github.com/torvalds/linux.git)
    #[arg(default_value = "https://github.com/torvalds/linux.git")]
    repo_url: String,

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

    /// Commit message for generated commit
    #[arg(long, default_value = "Changes made via git-nfs proxy")]
    commit_message: String,

    /// Author signature (e.g. "User Name <user@example.com>")
    #[arg(long)]
    author: Option<String>,

    /// Run the server only without executing mount_nfs
    #[arg(long)]
    no_mount: bool,
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
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!("=== Git NFS Proxy Server (Read-Write) ===");
    info!("Repository URL : {}", args.repo_url);
    info!("Mountpoint     : {}", args.mountpoint.display());
    info!("Branch / Ref   : {:?}", args.branch.as_deref().unwrap_or("HEAD"));

    // Determine cache and staging directory
    let base_cache_dir = if let Some(ref dir) = args.cache_dir {
        dir.clone()
    } else {
        let mut hasher = Sha1::new();
        hasher.update(args.repo_url.as_bytes());
        let repo_slug = hex::encode(hasher.finalize());
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".cache").join("git-nfs").join(repo_slug)
    };

    // 1. Initialize Staging Store
    let staging = Arc::new(StagingStore::new(&base_cache_dir).context("Initializing Staging Store")?);

    // 2. Initialize Git Engine
    let git_engine = Arc::new(
        GitEngine::new(&args.repo_url, Some(&base_cache_dir))
            .context("Initializing Git Engine")?,
    );

    // 3. Fetch repository commit and tree hierarchy
    info!("Connecting to remote Git repository...");
    let root_oid = git_engine
        .initialize(args.branch.as_deref())
        .await
        .context("Failed to initialize remote Git tree hierarchy")?;

    let base_commit_oid = git_engine
        .base_commit_oid()
        .ok_or_else(|| anyhow::anyhow!("Base commit OID unavailable"))?;

    // 4. Initialize VFS Manager
    let vfs = Arc::new(VfsManager::new(git_engine.clone(), &root_oid));

    // 5. Create Read-Write NFS File System
    let nfs_fs = GitNfsFileSystem::new(vfs.clone(), git_engine.clone(), staging.clone());

    // 6. Start NFS Server
    let server_info = start_nfs_server("127.0.0.1", args.port, nfs_fs)
        .await
        .context("Starting NFS server")?;

    info!(
        "NFS server is active on {}:{}",
        server_info.ip, server_info.port
    );

    // 7. Mount filesystem
    let mounter = if !args.no_mount {
        Some(
            NfsMounter::mount(&server_info.ip, server_info.port, &args.mountpoint)
                .context("Mounting NFS share")?,
        )
    } else {
        info!("Skipping mount as --no-mount was passed.");
        None
    };

    println!();
    println!("🎉 Git repository successfully mounted at: {}", args.mountpoint.display());
    println!("👉 You can now browse & EDIT it with Finder or Terminal: open {}", args.mountpoint.display());
    println!("👉 Press Ctrl+C to unmount and automatically generate the Git packfile of your changes.");
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

    // Cleanly unmount first so no more writes arrive
    if let Some(m) = mounter {
        m.unmount();
    }

    // Check if any modifications were made
    if staging.has_modifications() {
        println!();
        info!("Changes detected! Generating Git commit and packfile...");

        let author = args.author.unwrap_or_else(resolve_author);
        let (new_commit_oid, objects) = rebuild_git_objects(
            &vfs,
            &staging,
            &git_engine,
            &base_commit_oid,
            &author,
            &args.commit_message,
        )
        .await
        .context("Rebuilding Git tree objects")?;

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let pack_path = args
            .output_pack
            .unwrap_or_else(|| cwd.join(format!("changes-{new_commit_oid}.pack")));
        let idx_path = pack_path.with_extension("idx");

        PackWriter::write_pack_and_index(&pack_path, &idx_path, &objects)
            .context("Writing changes packfile")?;

        println!();
        println!("===============================================================");
        println!("🎉 Successfully generated Git packfile containing your changes!");
        println!("===============================================================");
        println!("📌 New Commit SHA : {new_commit_oid}");
        println!("📌 Base Commit SHA: {base_commit_oid}");
        println!("📦 Packfile       : {}", pack_path.display());
        println!("📄 Index file     : {}", idx_path.display());
        println!("📊 Total objects  : {}", objects.len());
        println!();
        println!("To inspect your packfile:");
        println!("  git verify-pack -v {}", pack_path.display());
        println!("===============================================================");
    } else {
        info!("No changes were made. Clean exit.");
    }

    Ok(())
}
