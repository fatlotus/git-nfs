use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::RwLock;
use tracing::debug;

pub struct StagingStore {
    staging_dir: PathBuf,
    // Track nodes that have been modified or created: node_id -> file size
    staged_nodes: RwLock<HashMap<u64, u64>>,
    // Track nodes that have been deleted: node_id
    deleted_nodes: RwLock<HashSet<u64>>,
    // Track repository-relative paths modified, created, or deleted
    changed_paths: RwLock<HashSet<String>>,
}

impl StagingStore {
    pub fn new(base_cache_dir: &Path) -> Result<Self> {
        let staging_dir = base_cache_dir.join("staging");
        if staging_dir.exists() {
            let _ = fs::remove_dir_all(&staging_dir);
        }
        fs::create_dir_all(&staging_dir).context("Creating staging directory")?;

        Ok(Self {
            staging_dir,
            staged_nodes: RwLock::new(HashMap::new()),
            deleted_nodes: RwLock::new(HashSet::new()),
            changed_paths: RwLock::new(HashSet::new()),
        })
    }

    fn file_path(&self, node_id: u64) -> PathBuf {
        self.staging_dir.join(format!("node_{node_id}.bin"))
    }

    pub fn is_staged(&self, node_id: u64) -> bool {
        self.staged_nodes.read().contains_key(&node_id)
    }

    pub fn is_deleted(&self, node_id: u64) -> bool {
        self.deleted_nodes.read().contains(&node_id)
    }

    pub fn list_deleted_nodes(&self) -> Vec<u64> {
        self.deleted_nodes.read().iter().copied().collect()
    }

    pub fn mark_deleted(&self, node_id: u64) {
        self.deleted_nodes.write().insert(node_id);
    }

    pub fn get_size(&self, node_id: u64) -> Option<u64> {
        self.staged_nodes.read().get(&node_id).copied()
    }

    /// Initializes a newly created file in staging with 0 bytes.
    pub fn create_file(&self, node_id: u64) -> Result<()> {
        let path = self.file_path(node_id);
        File::create(&path).context("Creating new staged file")?;
        self.staged_nodes.write().insert(node_id, 0);
        debug!("Created staged file for node {node_id}");
        Ok(())
    }

    /// Writes data to a staged file at a specific offset.
    /// If the file is not yet in staging, `initial_data` is used as base content.
    pub fn write_at(
        &self,
        node_id: u64,
        offset: u64,
        data: &[u8],
        initial_data: Option<&[u8]>,
    ) -> Result<u64> {
        let path = self.file_path(node_id);

        if !path.exists() {
            let mut file = File::create(&path).context("Creating staged file on first write")?;
            if let Some(base) = initial_data {
                file.write_all(base).context("Writing initial blob to staging")?;
            }
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .context("Opening staged file for write")?;

        file.seek(SeekFrom::Start(offset))
            .context("Seeking in staged file")?;
        file.write_all(data).context("Writing data to staged file")?;
        file.flush()?;

        let current_len = file.metadata()?.len();
        self.staged_nodes.write().insert(node_id, current_len);

        debug!(
            "Wrote {} bytes to staged node {} at offset {} (new size: {})",
            data.len(),
            node_id,
            offset,
            current_len
        );

        Ok(current_len)
    }

    /// Truncates or extends a staged file.
    pub fn truncate(
        &self,
        node_id: u64,
        new_size: u64,
        initial_data: Option<&[u8]>,
    ) -> Result<()> {
        let path = self.file_path(node_id);

        if !path.exists() {
            let mut file = File::create(&path).context("Creating staged file for truncate")?;
            if let Some(base) = initial_data {
                file.write_all(base).context("Writing initial blob to staging")?;
            }
        }

        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .context("Opening staged file for set_len")?;

        file.set_len(new_size).context("Setting staged file len")?;
        self.staged_nodes.write().insert(node_id, new_size);

        debug!("Truncated staged node {node_id} to {new_size} bytes");
        Ok(())
    }

    /// Reads data from a staged file.
    pub fn read_at(&self, node_id: u64, offset: u64, count: u32) -> Result<Option<(Vec<u8>, bool)>> {
        if !self.is_staged(node_id) {
            return Ok(None);
        }

        let path = self.file_path(node_id);
        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };

        let file_len = file.metadata()?.len();
        if offset >= file_len {
            return Ok(Some((Vec::new(), true)));
        }

        file.seek(SeekFrom::Start(offset))?;
        let to_read = (count as u64).min(file_len - offset) as usize;
        let mut buf = vec![0u8; to_read];
        file.read_exact(&mut buf)?;

        let eof = (offset + to_read as u64) >= file_len;
        Ok(Some((buf, eof)))
    }

    /// Returns the full content of a staged node.
    pub fn get_staged_content(&self, node_id: u64) -> Result<Option<Vec<u8>>> {
        if !self.is_staged(node_id) {
            return Ok(None);
        }
        let path = self.file_path(node_id);
        let data = fs::read(&path)?;
        Ok(Some(data))
    }

    /// Returns list of all staged node IDs.
    pub fn list_staged_nodes(&self) -> Vec<u64> {
        self.staged_nodes.read().keys().copied().collect()
    }

    /// Records a repository-relative path that was modified, created, or deleted.
    pub fn record_changed_path(&self, path: &str) {
        let clean = path.trim().trim_matches('/');
        if !clean.is_empty() {
            self.changed_paths.write().insert(clean.to_string());
        }
    }

    /// Returns a list of all recorded changed paths.
    pub fn list_changed_paths(&self) -> Vec<String> {
        self.changed_paths.read().iter().cloned().collect()
    }

    /// Checks if any modifications exist.
    pub fn has_modifications(&self) -> bool {
        !self.staged_nodes.read().is_empty()
            || !self.deleted_nodes.read().is_empty()
            || !self.changed_paths.read().is_empty()
    }

    /// Clears staged nodes, deleted nodes, and changed paths, and cleans up
    /// all staged files on disk so subsequent edits begin with a clean staging area.
    pub fn reset(&self) -> Result<()> {
        self.staged_nodes.write().clear();
        self.deleted_nodes.write().clear();
        self.changed_paths.write().clear();

        if self.staging_dir.exists() {
            let _ = fs::remove_dir_all(&self.staging_dir);
        }
        fs::create_dir_all(&self.staging_dir).context("Re-creating clean staging directory")?;

        Ok(())
    }
}
