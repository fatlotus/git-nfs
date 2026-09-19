pub mod builder;
pub mod pack;
pub mod pack_writer;
pub mod protocol;
pub mod smart_http;
pub mod tree;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use tracing::info;

use crate::cache::disk::DiskCache;
use crate::git::pack::{unpack_packfile, ObjectType};
use crate::git::smart_http::GitSmartHttpClient;
use crate::git::tree::GitTree;

pub struct GitEngine {
    http_client: GitSmartHttpClient,
    disk_cache: Arc<DiskCache>,
    // In-memory tree and commit objects: OID -> parsed GitTree
    trees: RwLock<HashMap<String, Arc<GitTree>>>,
    // Root commit OID and root tree OID
    root_tree_oid: RwLock<Option<String>>,
    commit_oid: RwLock<Option<String>>,
}

impl GitEngine {
    pub fn new(repo_url: &str, cache_dir: Option<&std::path::Path>) -> Result<Self> {
        let http_client = GitSmartHttpClient::new(repo_url);
        let disk_cache = Arc::new(DiskCache::new(repo_url, cache_dir)?);

        Ok(Self {
            http_client,
            disk_cache,
            trees: RwLock::new(HashMap::new()),
            root_tree_oid: RwLock::new(None),
            commit_oid: RwLock::new(None),
        })
    }

    /// Initializes the GitEngine: resolves HEAD, fetches the commit and all trees.
    pub async fn initialize(&self, branch: Option<&str>) -> Result<String> {
        let (commit_oid, _branch_name) = self.http_client.resolve_head(branch).await?;
        *self.commit_oid.write() = Some(commit_oid.clone());

        // Fetch all trees in one packfile
        let pack_bytes = self.http_client.fetch_trees_pack(&commit_oid).await?;
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

    pub fn get_root_tree_oid(&self) -> Option<String> {
        self.root_tree_oid.read().clone()
    }

    pub fn base_commit_oid(&self) -> Option<String> {
        self.commit_oid.read().clone()
    }

    pub fn get_tree(&self, oid: &str) -> Option<Arc<GitTree>> {
        self.trees.read().get(oid).cloned()
    }

    /// Gets a blob's content by OID:
    /// 1. Checks on-disk blob cache
    /// 2. If missing, lazily fetches via Git Protocol v2, unpacks, and stores to disk cache
    pub async fn get_blob(&self, blob_oid: &str) -> Result<Vec<u8>> {
        // 1. Check disk cache
        if let Some(data) = self.disk_cache.get_blob(blob_oid) {
            return Ok(data);
        }

        // 2. Fetch from remote
        let pack_bytes = self.http_client.fetch_blob_pack(blob_oid).await?;
        let objects = unpack_packfile(&pack_bytes).context("Unpacking blob packfile")?;

        if let Some(blob_obj) = objects.get(blob_oid) {
            self.disk_cache.put_blob(blob_oid, &blob_obj.data)?;
            Ok(blob_obj.data.clone())
        } else if let Some((_, first_obj)) = objects.into_iter().next() {
            // Some servers might return delta or base with different sha or matching single obj
            self.disk_cache.put_blob(blob_oid, &first_obj.data)?;
            Ok(first_obj.data)
        } else {
            Err(anyhow!("Requested blob {blob_oid} not in packfile"))
        }
    }
}
