use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info};

use crate::wal::backend::{ActiveWalWriter, WalBackend};
use crate::wal::proto::WalEntry;

pub struct LocalActiveWalWriter {
    file: File,
}

#[async_trait]
impl ActiveWalWriter for LocalActiveWalWriter {
    async fn append_entry(&mut self, entry: &WalEntry) -> Result<()> {
        let framed = entry.encode_framed();
        self.file
            .write_all(&framed)
            .context("Writing entry to local WAL file")?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.file.flush().context("Flushing local WAL file")?;
        self.file.sync_data().context("Syncing local WAL file")?;
        Ok(())
    }

    async fn finalize(mut self: Box<Self>) -> Result<()> {
        self.flush().await?;
        Ok(())
    }
}

pub struct LocalWalBackend {
    dir: PathBuf,
}

impl LocalWalBackend {
    pub fn new(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("Creating local WAL dir at {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn log_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.log"))
    }

    fn done_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.done"))
    }

    fn pack_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.pack"))
    }

    fn idx_path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.idx"))
    }
}

#[async_trait]
impl WalBackend for LocalWalBackend {
    async fn list_log_sequences(&self) -> Result<Vec<u64>> {
        let mut seqs = Vec::new();
        if !self.dir.exists() {
            return Ok(seqs);
        }

        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(num_str) = name_str.strip_suffix(".log") {
                if let Ok(seq) = num_str.parse::<u64>() {
                    seqs.push(seq);
                }
            }
        }

        seqs.sort_unstable();
        Ok(seqs)
    }

    async fn is_log_done(&self, seq: u64) -> Result<bool> {
        Ok(self.done_path(seq).exists())
    }

    async fn mark_log_done(
        &self,
        seq: u64,
        new_commit_oid: &str,
        pack_data: Option<&[u8]>,
        idx_data: Option<&[u8]>,
    ) -> Result<()> {
        let done_file = self.done_path(seq);
        fs::write(&done_file, format!("{new_commit_oid}\n"))
            .with_context(|| format!("Writing done marker for WAL {seq}"))?;

        if let Some(pack) = pack_data {
            fs::write(self.pack_path(seq), pack)
                .with_context(|| format!("Writing packfile for WAL {seq}"))?;
        }
        if let Some(idx) = idx_data {
            fs::write(self.idx_path(seq), idx)
                .with_context(|| format!("Writing idx for WAL {seq}"))?;
        }

        info!("Marked local WAL {seq} as completed (commit {new_commit_oid})");
        Ok(())
    }

    async fn lockout_zombies(&self, previous_seqs: &[u64]) -> Result<()> {
        for &seq in previous_seqs {
            let path = self.log_path(seq);
            if path.exists() {
                // Set read-only permissions to prevent zombie writers
                if let Ok(metadata) = fs::metadata(&path) {
                    let mut perms = metadata.permissions();
                    perms.set_readonly(true);
                    let _ = fs::set_permissions(&path, perms);
                }
            }
        }
        Ok(())
    }

    async fn open_writer(&self, seq: u64) -> Result<Box<dyn ActiveWalWriter>> {
        let path = self.log_path(seq);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Opening local WAL writer at {}", path.display()))?;

        debug!("Opened local WAL {seq} at {}", path.display());
        Ok(Box::new(LocalActiveWalWriter { file }))
    }

    async fn read_log(&self, seq: u64) -> Result<Vec<u8>> {
        let path = self.log_path(seq);
        let mut file = File::open(&path)
            .with_context(|| format!("Reading local WAL file {}", path.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}
