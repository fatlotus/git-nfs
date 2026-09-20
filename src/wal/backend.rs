use anyhow::Result;
use async_trait::async_trait;

use crate::wal::proto::WalEntry;

#[async_trait]
pub trait ActiveWalWriter: Send + Sync {
    /// Appends a framed WalEntry into the active log.
    async fn append_entry(&mut self, entry: &WalEntry) -> Result<()>;

    /// Flushes appended bytes to durable storage.
    async fn flush(&mut self) -> Result<()>;

    /// Finalizes the log, sealing it against future appends.
    async fn finalize(self: Box<Self>) -> Result<()>;
}

#[async_trait]
pub trait WalBackend: Send + Sync {
    /// Discovers existing sequential log IDs (e.g. 0 from 0.log, 1 from 1.log).
    async fn list_log_sequences(&self) -> Result<Vec<u64>>;

    /// Checks if a log sequence has already been marked completed (e.g. seq.done exists).
    async fn is_log_done(&self, seq: u64) -> Result<bool>;

    /// Marks a log sequence as completed, and optionally saves the recovered/generated
    /// pack and index files into the storage backend.
    async fn mark_log_done(
        &self,
        seq: u64,
        new_commit_oid: &str,
        pack_data: Option<&[u8]>,
        idx_data: Option<&[u8]>,
    ) -> Result<()>;

    /// Finalizes all previous logs to lock out any zombie NFS servers.
    async fn lockout_zombies(&self, previous_seqs: &[u64]) -> Result<()>;

    /// Opens the specified sequence as the new active appendable log writer.
    async fn open_writer(&self, seq: u64) -> Result<Box<dyn ActiveWalWriter>>;

    /// Reads the full byte content of a log for replay.
    async fn read_log(&self, seq: u64) -> Result<Vec<u8>>;
}
