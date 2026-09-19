pub mod builder;
pub mod pack;
pub mod pack_writer;
pub mod protocol;
pub mod smart_http;
pub mod tree;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;
use tracing::{debug, info};

use crate::cache::disk::DiskCache;
use crate::cache::memory::MemoryCache;
use crate::git::pack::{unpack_packfile, ObjectType};
use crate::git::smart_http::GitSmartHttpClient;
use crate::git::tree::GitTree;

pub struct GitEngine {
    http_client: GitSmartHttpClient,
    disk_cache: Arc<DiskCache>,
    memory_cache: Arc<MemoryCache>,
    // In-memory tree and commit objects: OID -> parsed GitTree
    trees: RwLock<HashMap<String, Arc<GitTree>>>,
    // Root commit OID and root tree OID
    root_tree_oid: RwLock<Option<String>>,
    commit_oid: RwLock<Option<String>>,
    // In-flight singleflight map to deduplicate concurrent blob fetches
    in_flight: Mutex<HashMap<String, Arc<Notify>>>,
}

impl GitEngine {
    pub fn new(repo_url: &str, cache_dir: Option<&std::path::Path>) -> Result<Self> {
        let http_client = GitSmartHttpClient::new(repo_url);
        let disk_cache = Arc::new(DiskCache::new(repo_url, cache_dir)?);
        // Default 2048 objects memory capacity
        let memory_cache = Arc::new(MemoryCache::new(2048));

        Ok(Self {
            http_client,
            disk_cache,
            memory_cache,
            trees: RwLock::new(HashMap::new()),
            root_tree_oid: RwLock::new(None),
            commit_oid: RwLock::new(None),
            in_flight: Mutex::new(HashMap::new()),
        })
    }

    #[allow(dead_code)]
    pub fn http_client(&self) -> &GitSmartHttpClient {
        &self.http_client
    }

    pub fn disk_cache(&self) -> &Arc<DiskCache> {
        &self.disk_cache
    }

    pub fn memory_cache(&self) -> &Arc<MemoryCache> {
        &self.memory_cache
    }

    /// Initializes the GitEngine: resolves HEAD, fetches the commit and all trees.
    /// Utilizes disk cache to avoid re-fetching tree hierarchy if already present.
    pub async fn initialize(&self, branch: Option<&str>) -> Result<String> {
        let (commit_oid, _branch_name) = self.http_client.resolve_head(branch).await?;
        *self.commit_oid.write() = Some(commit_oid.clone());

        // Check if tree packfile is already cached on disk
        let pack_bytes = if let Some(cached_pack) = self.disk_cache.get_tree_pack(&commit_oid) {
            info!(
                "Loaded tree hierarchy for commit {commit_oid} from disk cache ({} bytes)",
                cached_pack.len()
            );
            cached_pack
        } else {
            // Fetch all trees in one packfile over Git Protocol v2
            let pack = self.http_client.fetch_trees_pack(&commit_oid).await?;
            if let Err(e) = self.disk_cache.put_tree_pack(&commit_oid, &pack) {
                tracing::warn!("Failed to persist tree packfile to disk: {e}");
            }
            pack
        };

        let objects = unpack_packfile(&pack_bytes).context("Unpacking trees packfile")?;
        info!("Unpacked {} objects from repository trees", objects.len());

        let mut root_tree_oid = None;
        let mut trees = self.trees.write();

        // 1. Locate root tree from commit object
        if let Some(commit_obj) = objects.get(&commit_oid) {
            let commit_text = String::from_utf8_lossy(&commit_obj.data);
            for line in commit_text.lines() {
                if line.starts_with("tree ") {
                    let t_oid = line.trim_start_matches("tree ").trim().to_string();
                    root_tree_oid = Some(t_oid);
                    break;
                }
            }
        }

        // 2. Parse all tree objects
        for (oid, obj) in &objects {
            if obj.obj_type == ObjectType::Tree {
                match GitTree::parse(&obj.data) {
                    Ok(tree) => {
                        trees.insert(oid.clone(), Arc::new(tree));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse tree {oid}: {e}");
                    }
                }
            }
        }

        let root_oid = root_tree_oid.ok_or_else(|| {
            anyhow!("Could not find root tree in commit object {commit_oid}")
        })?;

        info!("Root tree OID: {root_oid}");
        *self.root_tree_oid.write() = Some(root_oid.clone());

        Ok(root_oid)
    }

    #[allow(dead_code)]
    pub fn get_root_tree_oid(&self) -> Option<String> {
        self.root_tree_oid.read().clone()
    }

    pub fn base_commit_oid(&self) -> Option<String> {
        self.commit_oid.read().clone()
    }

    pub fn get_tree(&self, oid: &str) -> Option<Arc<GitTree>> {
        self.trees.read().get(oid).cloned()
    }

    /// Prefetches and caches all un-cached file blobs in a directory tree in batches.
    /// This drastically minimizes HTTP requests to GitHub (e.g. 1 request for all root files).
    pub async fn prefetch_directory_blobs(&self, tree_oid: &str) -> Result<usize> {
        let tree = match self.get_tree(tree_oid) {
            Some(t) => t,
            None => return Ok(0),
        };

        // Collect OIDs of regular files or symlinks that are not yet cached
        let mut missing_oids = Vec::new();
        for entry in &tree.entries {
            if !entry.mode.is_dir() {
                let oid = &entry.oid;
                if self.memory_cache.get(oid).is_none() && !self.disk_cache.has_blob(oid) {
                    missing_oids.push(oid.as_str());
                }
            }
        }

        if missing_oids.is_empty() {
            return Ok(0);
        }

        info!(
            "Prefetching {} missing blobs for tree {} in batches...",
            missing_oids.len(),
            tree_oid
        );

        let mut total_fetched = 0;
        // Batch in groups of up to 64 objects per request
        for chunk in missing_oids.chunks(64) {
            let pack_bytes = self.http_client.fetch_blobs_pack(chunk).await?;
            if pack_bytes.is_empty() {
                continue;
            }
            let objects = unpack_packfile(&pack_bytes).context("Unpacking prefetched blobs")?;

            for (oid, obj) in objects {
                let arc_data = Arc::new(obj.data);
                self.memory_cache.put(&oid, arc_data.clone());
                if let Err(e) = self.disk_cache.put_blob(&oid, &arc_data) {
                    tracing::warn!("Failed to cache blob {oid} to disk: {e}");
                }
                total_fetched += 1;
            }
        }

        debug!("Prefetched {total_fetched} blobs for tree {tree_oid}");
        Ok(total_fetched)
    }

    /// Gets a blob's content by OID as Arc<Vec<u8>>:
    /// 1. Checks in-memory LRU cache
    /// 2. Checks on-disk blob cache
    /// 3. If missing, dedupes in-flight requests and fetches via Git Protocol v2
    pub async fn get_blob_arc(&self, blob_oid: &str) -> Result<Arc<Vec<u8>>> {
        // 1. Check in-memory LRU cache
        if let Some(data) = self.memory_cache.get(blob_oid) {
            return Ok(data);
        }

        // 2. Check disk cache
        if let Some(data) = self.disk_cache.get_blob(blob_oid) {
            let arc_data = Arc::new(data);
            self.memory_cache.put(blob_oid, arc_data.clone());
            return Ok(arc_data);
        }

        // 3. Coordinate with in-flight fetchers (singleflight)
        let notify = {
            let mut in_flight = self.in_flight.lock();
            if let Some(notify) = in_flight.get(blob_oid) {
                Some(notify.clone())
            } else {
                let notify = Arc::new(Notify::new());
                in_flight.insert(blob_oid.to_string(), notify);
                None
            }
        };

        if let Some(notify) = notify {
            // Wait for existing fetcher to complete
            notify.notified().await;
            // Now check memory and disk again
            if let Some(data) = self.memory_cache.get(blob_oid) {
                return Ok(data);
            }
            if let Some(data) = self.disk_cache.get_blob(blob_oid) {
                let arc_data = Arc::new(data);
                self.memory_cache.put(blob_oid, arc_data.clone());
                return Ok(arc_data);
            }
        }

        // This task is the fetcher
        let fetch_res = async {
            let pack_bytes = self.http_client.fetch_blob_pack(blob_oid).await?;
            let objects = unpack_packfile(&pack_bytes).context("Unpacking blob packfile")?;

            let mut matched_data = None;
            for (oid, obj) in objects {
                let arc_data = Arc::new(obj.data);
                self.memory_cache.put(&oid, arc_data.clone());
                let _ = self.disk_cache.put_blob(&oid, &arc_data);
                if oid == blob_oid {
                    matched_data = Some(arc_data);
                }
            }

            if let Some(data) = matched_data {
                Ok(data)
            } else if let Some(data) = self.memory_cache.get(blob_oid) {
                Ok(data)
            } else {
                Err(anyhow!("Requested blob {blob_oid} not in packfile"))
            }
        }
        .await;

        // Clean up in-flight map and notify all waiters
        let waiter = {
            let mut in_flight = self.in_flight.lock();
            in_flight.remove(blob_oid)
        };
        if let Some(w) = waiter {
            w.notify_waiters();
        }

        fetch_res
    }

    /// Gets a blob's content by OID as Vec<u8>.
    pub async fn get_blob(&self, blob_oid: &str) -> Result<Vec<u8>> {
        let arc_data = self.get_blob_arc(blob_oid).await?;
        Ok((*arc_data).clone())
    }
}
