pub mod builder;
pub mod gcs_storage;
pub mod idx;
pub mod pack;
pub mod pack_writer;
pub mod protocol;
pub mod smart_http;
pub mod tree;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::{Mutex, RwLock};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::cache::disk::DiskCache;
use crate::cache::memory::MemoryCache;
pub use crate::git::gcs_storage::GcsGitStorage;
use crate::git::pack::{unpack_packfile, ObjectType};
use crate::git::smart_http::GitSmartHttpClient;
use crate::git::tree::GitTree;

pub enum GitSource {
    Http(GitSmartHttpClient),
    Gcs(Arc<GcsGitStorage>),
}

pub struct GitEngine {
    source: GitSource,
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
    pub fn new(repo_url: &str, cache_dir: Option<&Path>) -> Result<Self> {
        Self::new_http(repo_url, cache_dir)
    }

    pub fn new_http(repo_url: &str, cache_dir: Option<&Path>) -> Result<Self> {
        let http_client = GitSmartHttpClient::new(repo_url);
        let disk_cache = Arc::new(DiskCache::new(repo_url, cache_dir)?);
        let memory_cache = Arc::new(MemoryCache::new(2048));

        Ok(Self {
            source: GitSource::Http(http_client),
            disk_cache,
            memory_cache,
            trees: RwLock::new(HashMap::new()),
            root_tree_oid: RwLock::new(None),
            commit_oid: RwLock::new(None),
            in_flight: Mutex::new(HashMap::new()),
        })
    }

    pub async fn new_gcs(
        bucket: &str,
        prefix: &str,
        cache_dir: Option<&Path>,
    ) -> Result<Self> {
        let effective_cache_dir = if let Some(dir) = cache_dir {
            dir.to_path_buf()
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            Path::new(&home).join(".cache").join("git-nfs").join(format!("gcs_{bucket}"))
        };

        let gcs_storage = Arc::new(GcsGitStorage::new(bucket, prefix, &effective_cache_dir).await?);
        let disk_cache = Arc::new(DiskCache::new(&format!("gs://{bucket}/{prefix}"), cache_dir)?);
        let memory_cache = Arc::new(MemoryCache::new(4096));

        Ok(Self {
            source: GitSource::Gcs(gcs_storage),
            disk_cache,
            memory_cache,
            trees: RwLock::new(HashMap::new()),
            root_tree_oid: RwLock::new(None),
            commit_oid: RwLock::new(None),
            in_flight: Mutex::new(HashMap::new()),
        })
    }

    pub async fn new_auto(repo_target: &str, cache_dir: Option<&Path>) -> Result<Self> {
        if repo_target.starts_with("gs://") {
            let without = &repo_target["gs://".len()..];
            let (bucket, prefix) = match without.split_once('/') {
                Some((b, p)) => (b, p),
                None => (without, ""),
            };
            Self::new_gcs(bucket, prefix, cache_dir).await
        } else {
            Self::new_http(repo_target, cache_dir)
        }
    }

    pub fn http_client(&self) -> Option<&GitSmartHttpClient> {
        match &self.source {
            GitSource::Http(client) => Some(client),
            _ => None,
        }
    }

    pub fn gcs_storage(&self) -> Option<Arc<GcsGitStorage>> {
        match &self.source {
            GitSource::Gcs(storage) => Some(storage.clone()),
            _ => None,
        }
    }

    pub fn disk_cache(&self) -> &Arc<DiskCache> {
        &self.disk_cache
    }

    pub fn memory_cache(&self) -> &Arc<MemoryCache> {
        &self.memory_cache
    }

    /// Initializes the GitEngine: resolves HEAD, fetches the commit and all trees.
    pub async fn initialize(&self, branch: Option<&str>) -> Result<String> {
        match &self.source {
            GitSource::Http(client) => {
                let (commit_oid, _branch_name) = client.resolve_head(branch).await?;
                *self.commit_oid.write() = Some(commit_oid.clone());

                // Check if tree packfile is already cached on disk
                let pack_bytes = if let Some(cached_pack) = self.disk_cache.get_tree_pack(&commit_oid) {
                    info!(
                        "Loaded tree hierarchy for commit {commit_oid} from disk cache ({} bytes)",
                        cached_pack.len()
                    );
                    cached_pack
                } else {
                    let pack = client.fetch_trees_pack(&commit_oid).await?;
                    if let Err(e) = self.disk_cache.put_tree_pack(&commit_oid, &pack) {
                        warn!("Failed to persist tree packfile to disk: {e}");
                    }
                    pack
                };

                let objects = unpack_packfile(&pack_bytes).context("Unpacking trees packfile")?;
                info!("Unpacked {} objects from repository trees", objects.len());

                let mut root_tree_oid = None;
                let mut trees = self.trees.write();

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

                for (oid, obj) in &objects {
                    if obj.obj_type == ObjectType::Tree {
                        if let Ok(tree) = GitTree::parse(&obj.data) {
                            trees.insert(oid.clone(), Arc::new(tree));
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
            GitSource::Gcs(storage) => {
                let (commit_oid, _branch_name) = storage.resolve_head(branch).await?;
                *self.commit_oid.write() = Some(commit_oid.clone());

                // Pre-cache all packfiles locally to enable instantaneous disk reads
                let pack_names = storage.pack_names();

                for pack_name in &pack_names {
                    if let Err(e) = storage.ensure_pack_cached(pack_name).await {
                        warn!("Could not cache pack {pack_name} (will use range reads): {e}");
                    }
                }

                // Read commit object to get root tree OID
                let commit_raw = storage
                    .read_object_raw(&commit_oid)
                    .await
                    .with_context(|| format!("Reading commit object {commit_oid} from GCS"))?;

                let mut root_tree_oid = None;
                for line in String::from_utf8_lossy(&commit_raw.data).lines() {
                    if line.starts_with("tree ") {
                        let t_oid = line.trim_start_matches("tree ").trim().to_string();
                        root_tree_oid = Some(t_oid);
                        break;
                    }
                }

                let root_oid = root_tree_oid
                    .ok_or_else(|| anyhow!("Could not find root tree in commit {commit_oid}"))?;
                info!("Root tree OID from GCS: {root_oid}");

                // Recursively traverse and parse all trees reachable from root
                info!("Loading tree hierarchy from GCS packfiles...");
                let mut queue = VecDeque::new();
                queue.push_back(root_oid.clone());

                let mut loaded_trees = 0;
                while let Some(tree_oid) = queue.pop_front() {
                    if self.trees.read().contains_key(&tree_oid) {
                        continue;
                    }

                    match storage.read_object_raw(&tree_oid).await {
                        Ok(raw_tree) => match GitTree::parse(&raw_tree.data) {
                            Ok(parsed) => {
                                for entry in &parsed.entries {
                                    if entry.mode.is_dir()
                                        && !self.trees.read().contains_key(&entry.oid)
                                    {
                                        queue.push_back(entry.oid.clone());
                                    }
                                }
                                self.trees.write().insert(tree_oid, Arc::new(parsed));
                                loaded_trees += 1;
                            }
                            Err(e) => {
                                warn!("Failed to parse tree {tree_oid}: {e}");
                            }
                        },
                        Err(e) => {
                            warn!("Failed to read tree object {tree_oid}: {e}");
                        }
                    }
                }

                info!("Successfully loaded {loaded_trees} trees into memory from GCS repository");
                *self.root_tree_oid.write() = Some(root_oid.clone());

                Ok(root_oid)
            }
        }
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
    pub async fn prefetch_directory_blobs(&self, tree_oid: &str) -> Result<usize> {
        let tree = match self.get_tree(tree_oid) {
            Some(t) => t,
            None => return Ok(0),
        };

        let mut missing_oids = Vec::new();
        for entry in &tree.entries {
            if !entry.mode.is_dir() {
                let oid = &entry.oid;
                if self.memory_cache.get(oid).is_none() && !self.disk_cache.has_blob(oid) {
                    missing_oids.push(oid.clone());
                }
            }
        }

        if missing_oids.is_empty() {
            return Ok(0);
        }

        debug!(
            "Prefetching {} missing blobs for tree {}...",
            missing_oids.len(),
            tree_oid
        );

        let mut total_fetched = 0;

        match &self.source {
            GitSource::Http(client) => {
                let oids_ref: Vec<&str> = missing_oids.iter().map(|s| s.as_str()).collect();
                for chunk in oids_ref.chunks(64) {
                    let pack_bytes = client.fetch_blobs_pack(chunk).await?;
                    if pack_bytes.is_empty() {
                        continue;
                    }
                    let objects = unpack_packfile(&pack_bytes).context("Unpacking prefetched blobs")?;
                    for (oid, obj) in objects {
                        let arc_data = Arc::new(obj.data);
                        self.memory_cache.put(&oid, arc_data.clone());
                        let _ = self.disk_cache.put_blob(&oid, &arc_data);
                        total_fetched += 1;
                    }
                }
            }
            GitSource::Gcs(storage) => {
                for chunk in missing_oids.chunks(64) {
                    let mut tasks = Vec::new();
                    for oid in chunk {
                        let s = storage.clone();
                        let oid_clone = oid.clone();
                        tasks.push(tokio::spawn(async move {
                            s.read_object_raw(&oid_clone).await.map(|raw| (oid_clone, raw.data))
                        }));
                    }
                    for task in futures::future::join_all(tasks).await {
                        if let Ok(Ok((oid, data))) = task {
                            let arc_data = Arc::new(data);
                            self.memory_cache.put(&oid, arc_data.clone());
                            let _ = self.disk_cache.put_blob(&oid, &arc_data);
                            total_fetched += 1;
                        }
                    }
                }
            }
        }

        debug!("Prefetched {total_fetched} blobs for tree {tree_oid}");
        Ok(total_fetched)
    }

    /// Gets a blob's content by OID as Arc<Vec<u8>>:
    /// 1. Checks in-memory LRU cache
    /// 2. Checks on-disk blob cache
    /// 3. If missing, dedupes in-flight requests and fetches via Git source
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
            notify.notified().await;
            if let Some(data) = self.memory_cache.get(blob_oid) {
                return Ok(data);
            }
            if let Some(data) = self.disk_cache.get_blob(blob_oid) {
                let arc_data = Arc::new(data);
                self.memory_cache.put(blob_oid, arc_data.clone());
                return Ok(arc_data);
            }
        }

        // Fetcher task
        let fetch_res = async {
            match &self.source {
                GitSource::Http(client) => {
                    let pack_bytes = client.fetch_blob_pack(blob_oid).await?;
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
                GitSource::Gcs(storage) => {
                    let raw = storage.read_object_raw(blob_oid).await?;
                    let arc_data = Arc::new(raw.data);
                    self.memory_cache.put(blob_oid, arc_data.clone());
                    let _ = self.disk_cache.put_blob(blob_oid, &arc_data);
                    Ok(arc_data)
                }
            }
        }
        .await;

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

    /// Injects recovered Git objects (trees, blobs) from WAL crash recovery into
    /// the engine's tree table and caches, and advances the base commit and root tree.
    pub fn apply_recovered_objects(
        &self,
        commit_oid: &str,
        root_tree_oid: &str,
        objects: &[crate::git::builder::GitObjectToPack],
    ) {
        *self.commit_oid.write() = Some(commit_oid.to_string());
        *self.root_tree_oid.write() = Some(root_tree_oid.to_string());

        let mut trees = self.trees.write();
        for obj in objects {
            match obj.obj_type {
                ObjectType::Tree => {
                    if let Ok(tree) = GitTree::parse(&obj.data) {
                        trees.insert(obj.oid.clone(), Arc::new(tree));
                    }
                }
                ObjectType::Blob => {
                    let arc_data = Arc::new(obj.data.clone());
                    self.memory_cache.put(&obj.oid, arc_data.clone());
                    let _ = self.disk_cache.put_blob(&obj.oid, &arc_data);
                }
                _ => {}
            }
        }
    }
}
