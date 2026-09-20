use std::collections::HashMap;
use std::fs::{self, File};
use std::num::NonZeroUsize;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use lru::LruCache;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::{debug, info};

pub const DEFAULT_BLOCK_SIZE: usize = 10 * 1024 * 1024; // 10 MiB
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An on-demand 10 MiB block cache for Git packfiles.
///
/// Divides remote packfiles into 10 MiB chunks, caching them on local disk
/// and keeping recent blocks in an in-memory LRU cache. Avoids downloading
/// multi-gigabyte packfiles upfront.
pub struct BlockCache {
    cache_dir: PathBuf,
    block_size: usize,
    memory_blocks: Mutex<LruCache<(String, u64), Arc<Vec<u8>>>>,
    in_flight: Mutex<HashMap<(String, u64), Arc<Notify>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl BlockCache {
    pub fn new(cache_dir: PathBuf, block_size: usize) -> Self {
        let mem_cap = NonZeroUsize::new(32).unwrap(); // Keep up to 32 blocks (e.g. 320 MiB) in RAM
        Self {
            cache_dir,
            block_size: if block_size == 0 { DEFAULT_BLOCK_SIZE } else { block_size },
            memory_blocks: Mutex::new(LruCache::new(mem_cap)),
            in_flight: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Checks whether a block is already present in memory, on disk, or in full packfile.
    pub fn has_block(&self, pack_name: &str, block_index: u64) -> bool {
        let filename = self.extract_filename(pack_name);
        let key = (filename.to_string(), block_index);

        if self.memory_blocks.lock().contains(&key) {
            return true;
        }

        let path = self.block_path(pack_name, block_index);
        if path.is_file() {
            return true;
        }

        let full_path = self.full_pack_path(pack_name);
        if full_path.is_file() {
            return true;
        }

        false
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    fn extract_filename<'a>(&self, pack_name: &'a str) -> &'a str {
        pack_name.rsplit('/').next().unwrap_or(pack_name)
    }

    pub fn full_pack_path(&self, pack_name: &str) -> PathBuf {
        let filename = self.extract_filename(pack_name);
        self.cache_dir.join("objects").join("pack").join(filename)
    }

    pub fn block_path(&self, pack_name: &str, block_index: u64) -> PathBuf {
        let filename = self.extract_filename(pack_name);
        self.cache_dir
            .join("objects")
            .join("pack")
            .join(format!("{filename}.blocks"))
            .join(format!("block_{block_index}.bin"))
    }

    /// Reads an arbitrary byte range from a packfile.
    ///
    /// If the full packfile exists on disk, reads directly from it.
    /// Otherwise, fetches and stitches the necessary 10 MiB blocks using `fetch_block`.
    pub async fn read_range<F, Fut>(
        &self,
        pack_name: &str,
        offset: u64,
        len: usize,
        fetch_block: F,
    ) -> Result<Vec<u8>>
    where
        F: Fn(u64, usize) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>>>,
    {
        if len == 0 {
            return Ok(Vec::new());
        }

        // Fast-path: Check if the complete packfile already exists on disk
        let full_path = self.full_pack_path(pack_name);
        if full_path.is_file() {
            let file = File::open(&full_path)
                .with_context(|| format!("Opening cached packfile {}", full_path.display()))?;
            let file_len = file.metadata()?.len();
            if offset >= file_len {
                return Ok(Vec::new());
            }
            let to_read = std::cmp::min(len as u64, file_len - offset) as usize;
            let mut buf = vec![0u8; to_read];
            let read_bytes = file.read_at(&mut buf, offset)?;
            buf.truncate(read_bytes);
            return Ok(buf);
        }

        let block_size = self.block_size as u64;
        let start_block = offset / block_size;
        let end_block = (offset + (len as u64) - 1) / block_size;

        let mut result = Vec::with_capacity(len);

        for b in start_block..=end_block {
            let block_data = self.get_or_fetch_block(pack_name, b, &fetch_block).await?;
            let block_start_offset = b * block_size;

            let overlap_start = std::cmp::max(offset, block_start_offset) - block_start_offset;
            let overlap_end = std::cmp::min(
                offset + (len as u64),
                block_start_offset + (block_data.len() as u64),
            ) - block_start_offset;

            if overlap_start < block_data.len() as u64 {
                let s = overlap_start as usize;
                let e = std::cmp::min(overlap_end as usize, block_data.len());
                result.extend_from_slice(&block_data[s..e]);
            }

            // If the block is shorter than block_size, we've reached EOF of the packfile
            if block_data.len() < self.block_size {
                break;
            }
        }

        Ok(result)
    }

    /// Retrieves a block from memory LRU or disk cache, or fetches it via `fetch_block`.
    pub async fn get_or_fetch_block<F, Fut>(
        &self,
        pack_name: &str,
        block_index: u64,
        fetch_block: &F,
    ) -> Result<Arc<Vec<u8>>>
    where
        F: Fn(u64, usize) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>>>,
    {
        let filename = self.extract_filename(pack_name);
        let key = (filename.to_string(), block_index);

        // 1. Check in-memory LRU cache
        {
            let mut mem = self.memory_blocks.lock();
            if let Some(data) = mem.get(&key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(data.clone());
            }
        }

        let path = self.block_path(pack_name, block_index);

        // 2. Check on-disk block file
        if path.is_file() {
            if let Ok(data) = fs::read(&path) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                let arc_data = Arc::new(data);
                self.memory_blocks.lock().put(key, arc_data.clone());
                return Ok(arc_data);
            }
        }

        // 3. Coordinate in-flight downloads across concurrent requests
        loop {
            let notify = {
                let mut in_flight = self.in_flight.lock();
                if let Some(n) = in_flight.get(&key) {
                    Some(n.clone())
                } else {
                    let n = Arc::new(Notify::new());
                    in_flight.insert(key.clone(), n);
                    None
                }
            };

            if let Some(n) = notify {
                n.notified().await;

                // Check in-memory cache first
                if let Some(data) = self.memory_blocks.lock().get(&key) {
                    return Ok(data.clone());
                }

                // Check disk cache
                if let Ok(data) = fs::read(&path) {
                    let arc_data = Arc::new(data);
                    self.memory_blocks.lock().put(key.clone(), arc_data.clone());
                    return Ok(arc_data);
                }

                // If neither, another attempt may be needed
                continue;
            }

            break;
        }

        // We are the designated fetcher
        self.misses.fetch_add(1, Ordering::Relaxed);
        let block_offset = block_index * (self.block_size as u64);

        debug!(
            "Fetching pack block {block_index} for {pack_name} (offset {block_offset}, size {})",
            self.block_size
        );

        let fetch_res = fetch_block(block_offset, self.block_size).await;

        match fetch_res {
            Ok(data) => {
                // Persist block to disk atomically
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }

                let cnt = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
                let tmp_path = path.with_extension(format!("tmp.{}.{}", std::process::id(), cnt));

                if fs::write(&tmp_path, &data).is_ok() {
                    let _ = fs::rename(&tmp_path, &path);
                }

                info!(
                    "Cached pack block {block_index} ({} bytes) for {filename} -> {}",
                    data.len(),
                    path.display()
                );

                let arc_data = Arc::new(data);
                self.memory_blocks.lock().put(key.clone(), arc_data.clone());

                let waiter = {
                    let mut in_flight = self.in_flight.lock();
                    in_flight.remove(&key)
                };
                if let Some(w) = waiter {
                    w.notify_waiters();
                }

                Ok(arc_data)
            }
            Err(e) => {
                let waiter = {
                    let mut in_flight = self.in_flight.lock();
                    in_flight.remove(&key)
                };
                if let Some(w) = waiter {
                    w.notify_waiters();
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_block_cache_single_and_multi_block_read() -> Result<()> {
        let temp = TempDir::new()?;
        let block_size = 1000; // 1000 bytes per block for testing
        let cache = BlockCache::new(temp.path().to_path_buf(), block_size);

        // Simulated remote data: 3500 bytes total
        let mock_pack_data: Vec<u8> = (0..3500).map(|i| (i % 256) as u8).collect();
        let mock_clone = mock_pack_data.clone();

        let fetcher = |offset: u64, len: usize| {
            let data = mock_clone.clone();
            async move {
                let off = offset as usize;
                if off >= data.len() {
                    Ok(Vec::new())
                } else {
                    let end = std::cmp::min(off + len, data.len());
                    Ok(data[off..end].to_vec())
                }
            }
        };

        // 1. Read within block 0 (offset 100..250)
        let slice1 = cache
            .read_range("pack-123.pack", 100, 150, fetcher)
            .await?;
        assert_eq!(slice1.len(), 150);
        assert_eq!(slice1, &mock_pack_data[100..250]);

        // Verify block 0 is on disk
        let b0_path = cache.block_path("pack-123.pack", 0);
        assert!(b0_path.is_file());
        assert_eq!(fs::metadata(&b0_path)?.len(), 1000);

        // 2. Read crossing block boundary: block 0 into block 1 (offset 900..1200)
        let slice2 = cache
            .read_range("pack-123.pack", 900, 300, fetcher)
            .await?;
        assert_eq!(slice2.len(), 300);
        assert_eq!(slice2, &mock_pack_data[900..1200]);

        // Block 1 should now be on disk too
        let b1_path = cache.block_path("pack-123.pack", 1);
        assert!(b1_path.is_file());

        // 3. Read crossing multiple blocks: block 1 through block 3 (offset 1500..3200)
        let slice3 = cache
            .read_range("pack-123.pack", 1500, 1700, fetcher)
            .await?;
        assert_eq!(slice3.len(), 1700);
        assert_eq!(slice3, &mock_pack_data[1500..3200]);

        // 4. Verify hits and misses
        assert!(cache.hits() > 0);
        assert!(cache.misses() > 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_block_cache_reads_from_full_packfile_if_present() -> Result<()> {
        let temp = TempDir::new()?;
        let cache = BlockCache::new(temp.path().to_path_buf(), 1000);

        // Create full packfile
        let full_path = cache.full_pack_path("pack-full.pack");
        fs::create_dir_all(full_path.parent().unwrap())?;
        let full_data = vec![42u8; 5000];
        fs::write(&full_path, &full_data)?;

        // Fetcher should never be called
        let fetcher = |_offset: u64, _len: usize| async {
            panic!("Fetcher should not be called when full pack is present");
            #[allow(unreachable_code)]
            Ok(Vec::new())
        };

        let slice = cache.read_range("pack-full.pack", 50, 100, fetcher).await?;
        assert_eq!(slice.len(), 100);
        assert_eq!(slice, vec![42u8; 100]);

        Ok(())
    }
}
