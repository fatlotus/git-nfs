use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use tracing::debug;

pub struct DiskCache {
    base_dir: PathBuf,
}

impl DiskCache {
    pub fn new(repo_url: &str, custom_dir: Option<&Path>) -> Result<Self> {
        let base_dir = if let Some(dir) = custom_dir {
            dir.to_path_buf()
        } else {
            let mut hasher = Sha1::new();
            hasher.update(repo_url.as_bytes());
            let repo_slug = hex::encode(hasher.finalize());
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home)
                .join(".cache")
                .join("git-nfs")
                .join(repo_slug)
        };

        fs::create_dir_all(&base_dir).context("Creating cache directory")?;
        Ok(Self { base_dir })
    }

    fn blob_path(&self, oid: &str) -> PathBuf {
        if oid.len() >= 4 {
            self.base_dir
                .join("blobs")
                .join(&oid[..2])
                .join(&oid[2..4])
                .join(&oid[4..])
        } else {
            self.base_dir.join("blobs").join(oid)
        }
    }

    pub fn get_blob(&self, oid: &str) -> Option<Vec<u8>> {
        let path = self.blob_path(oid);
        match fs::read(&path) {
            Ok(data) => {
                debug!("Cache HIT for blob {oid} ({} bytes)", data.len());
                Some(data)
            }
            Err(_) => {
                debug!("Cache MISS for blob {oid}");
                None
            }
        }
    }

    pub fn put_blob(&self, oid: &str, data: &[u8]) -> Result<()> {
        let path = self.blob_path(oid);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, data).context("Writing blob to disk cache")?;
        debug!("Saved blob {oid} ({} bytes) to cache", data.len());
        Ok(())
    }
}
