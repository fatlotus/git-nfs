pub mod backend;
pub mod gcs;
pub mod local;
pub mod proto;
pub mod recovery;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::info;

pub use backend::{ActiveWalWriter, WalBackend};
pub use gcs::GcsRapidWalBackend;
pub use local::LocalWalBackend;
pub use proto::*;
pub use recovery::{build_pack_from_recovered_state, replay_wal_entries, RecoveredPackResult};

use crate::git::GitEngine;
use crate::staging::StagingStore;
use crate::vfs::inode::VfsManager;

pub struct WalManager {
    backend: Arc<dyn WalBackend>,
    active_seq: AtomicU64,
    active_writer: Arc<Mutex<Option<Box<dyn ActiveWalWriter>>>>,
    entry_seq: AtomicU64,
}

impl WalManager {
    pub fn new(backend: Arc<dyn WalBackend>, active_seq: u64) -> Self {
        Self {
            backend,
            active_seq: AtomicU64::new(active_seq),
            active_writer: Arc::new(Mutex::new(None)),
            entry_seq: AtomicU64::new(1),
        }
    }

    pub fn backend(&self) -> &Arc<dyn WalBackend> {
        &self.backend
    }

    pub fn active_seq(&self) -> u64 {
        self.active_seq.load(Ordering::SeqCst)
    }

    /// Advances to the next sequential WAL log and resets the entry counter.
    pub fn advance_seq(&self) -> u64 {
        self.entry_seq.store(1, Ordering::SeqCst);
        self.active_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Opens the active WAL writer for this session and writes the initial header.
    pub async fn start_active_wal(
        &self,
        repo_url: &str,
        base_commit_oid: &str,
        author: &str,
        commit_message: &str,
    ) -> Result<()> {
        let seq = self.active_seq.load(Ordering::SeqCst);
        let mut writer = self.backend.open_writer(seq).await?;

        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let header_entry = WalEntry {
            seq: self.entry_seq.fetch_add(1, Ordering::SeqCst),
            timestamp_unix_nanos: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            payload: Some(WalPayload::Header(WalHeader {
                repo_url: repo_url.to_string(),
                base_commit_oid: base_commit_oid.to_string(),
                author: author.to_string(),
                commit_message: commit_message.to_string(),
                started_at_unix_secs: now_secs,
                wal_seq: seq,
            })),
        };

        writer.append_entry(&header_entry).await?;
        writer.flush().await?;

        *self.active_writer.lock().await = Some(writer);
        info!("Active WAL {seq}.log initialized and durably flushed");
        Ok(())
    }

    /// Logs a filesystem mutation and immediately flushes to durable storage.
    pub async fn log_mutation(&self, payload: WalPayload) -> Result<()> {
        let entry = WalEntry {
            seq: self.entry_seq.fetch_add(1, Ordering::SeqCst),
            timestamp_unix_nanos: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            payload: Some(payload),
        };

        let mut lock = self.active_writer.lock().await;
        if let Some(ref mut writer) = *lock {
            writer.append_entry(&entry).await?;
            writer.flush().await?;
        }
        Ok(())
    }

    /// Finalizes the active WAL file, making it immutable.
    pub async fn finalize_active_wal(&self) -> Result<()> {
        let mut lock = self.active_writer.lock().await;
        if let Some(writer) = lock.take() {
            let seq = self.active_seq.load(Ordering::SeqCst);
            writer.finalize().await.context("Finalizing active WAL")?;
            info!("Finalized active WAL {seq}.log");
        }
        Ok(())
    }

    /// Checks for uncommitted previous WALs, locks out zombie servers,
    /// and replays uncommitted mutations to build a consolidated packfile.
    pub async fn recover_and_replay_uncommitted(
        backend: &Arc<dyn WalBackend>,
        git_engine: &Arc<GitEngine>,
        root_oid: &str,
        default_base_commit_oid: &str,
        default_author: &str,
        default_commit_message: &str,
        cache_dir: &std::path::Path,
        branch: Option<&str>,
    ) -> Result<Option<RecoveredPackResult>> {
        let all_seqs = backend.list_log_sequences().await?;
        if all_seqs.is_empty() {
            return Ok(None);
        }

        // 1. Lock out any previous zombie servers by finalizing all previous logs
        backend.lockout_zombies(&all_seqs).await?;

        // 2. Identify uncommitted sequences
        let mut uncommitted_seqs = Vec::new();
        for &seq in &all_seqs {
            if !backend.is_log_done(seq).await? {
                uncommitted_seqs.push(seq);
            }
        }

        if uncommitted_seqs.is_empty() {
            info!("All previous WALs are already marked complete. No crash recovery needed.");
            return Ok(None);
        }

        info!(
            "Detected {} uncommitted WAL(s) needing crash recovery: {:?}",
            uncommitted_seqs.len(),
            uncommitted_seqs
        );

        // 3. Set up fresh recovery VFS and StagingStore
        let recovery_staging_dir = cache_dir.join("recovery_staging");
        let recovery_staging = Arc::new(StagingStore::new(&recovery_staging_dir)?);
        let recovery_vfs = Arc::new(VfsManager::new(git_engine.clone(), root_oid));

        let mut base_commit_oid = default_base_commit_oid.to_string();
        let mut author = default_author.to_string();
        let mut commit_message = default_commit_message.to_string();

        // 4. Replay uncommitted WALs sequentially
        for &seq in &uncommitted_seqs {
            let log_bytes = backend.read_log(seq).await?;
            let (entries, consumed) = WalEntry::decode_all_framed(&log_bytes);
            info!(
                "Replaying WAL {seq}.log (parsed {} valid entries from {}/{} bytes)",
                entries.len(),
                consumed,
                log_bytes.len()
            );

            // Extract header metadata if present
            for entry in &entries {
                if let Some(WalPayload::Header(ref h)) = entry.payload {
                    if !h.base_commit_oid.is_empty() {
                        base_commit_oid = h.base_commit_oid.clone();
                    }
                    if !h.author.is_empty() {
                        author = h.author.clone();
                    }
                    if !h.commit_message.is_empty() {
                        commit_message = format!("Recovered from crash: {}", h.commit_message);
                    }
                }
            }

            replay_wal_entries(&entries, &recovery_vfs, &recovery_staging, git_engine).await?;
        }

        // 5. Build consolidated packfile from recovered state
        let pack_result = build_pack_from_recovered_state(
            &recovery_vfs,
            &recovery_staging,
            git_engine,
            &base_commit_oid,
            &author,
            &commit_message,
        )
        .await?;

        // 6. Update underlying Git repository and mark all recovered sequences as completed
        if let Some(ref res) = pack_result {
            // Write completed pack, index, and update refs on GCS BEFORE marking WAL done
            if let Some(gcs_storage) = git_engine.gcs_storage() {
                info!("Writing completed commit and packfile to underlying GCS Git repository...");
                gcs_storage
                    .write_completed_commit(
                        &res.commit_oid,
                        &res.pack_sha,
                        &res.pack_data,
                        &res.idx_data,
                        branch,
                    )
                    .await
                    .context("Publishing completed commit and packfile to GCS Git repository")?;
            }

            for &seq in &uncommitted_seqs {
                backend
                    .mark_log_done(
                        seq,
                        &res.commit_oid,
                        Some(&res.pack_data),
                        Some(&res.idx_data),
                    )
                    .await?;
            }
        } else {
            // No modifications made in these WALs; mark them done with empty commit
            for &seq in &uncommitted_seqs {
                backend.mark_log_done(seq, "empty", None, None).await?;
            }
        }

        // Clean up temporary recovery staging
        let _ = std::fs::remove_dir_all(&recovery_staging_dir);

        Ok(pack_result)
    }
}
