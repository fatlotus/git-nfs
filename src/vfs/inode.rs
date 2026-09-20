use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::RwLock;

use crate::git::tree::TreeEntryMode;
use crate::git::GitEngine;

pub const ROOT_INODE: u64 = 1;

#[derive(Debug)]
pub struct VfsNode {
    pub id: u64,
    pub parent_id: RwLock<u64>,
    pub name: RwLock<String>,
    pub oid: RwLock<String>,
    pub mode: TreeEntryMode,
    pub is_dir: bool,
    pub size: RwLock<u64>,
    pub is_deleted: RwLock<bool>,
}

pub struct VfsManager {
    git_engine: Arc<GitEngine>,
    nodes: RwLock<HashMap<u64, Arc<VfsNode>>>,
    child_map: RwLock<HashMap<(u64, String), u64>>,
    dir_children: RwLock<HashMap<u64, Vec<u64>>>,
    next_inode: AtomicU64,
}

impl VfsManager {
    pub fn new(git_engine: Arc<GitEngine>, root_oid: &str) -> Self {
        let root_node = Arc::new(VfsNode {
            id: ROOT_INODE,
            parent_id: RwLock::new(ROOT_INODE),
            name: RwLock::new("/".to_string()),
            oid: RwLock::new(root_oid.to_string()),
            mode: TreeEntryMode::Directory,
            is_dir: true,
            size: RwLock::new(4096),
            is_deleted: RwLock::new(false),
        });

        let mut nodes = HashMap::new();
        nodes.insert(ROOT_INODE, root_node);

        Self {
            git_engine,
            nodes: RwLock::new(nodes),
            child_map: RwLock::new(HashMap::new()),
            dir_children: RwLock::new(HashMap::new()),
            next_inode: AtomicU64::new(2),
        }
    }

    pub fn git_engine(&self) -> &Arc<GitEngine> {
        &self.git_engine
    }

    pub fn get_node(&self, id: u64) -> Option<Arc<VfsNode>> {
        let node = self.nodes.read().get(&id).cloned()?;
        if *node.is_deleted.read() {
            return None;
        }
        Some(node)
    }

    /// Populates children for a directory if not already populated.
    pub fn ensure_dir_populated(&self, dir_id: u64) -> Result<()> {
        if self.dir_children.read().contains_key(&dir_id) {
            return Ok(());
        }

        let dir_node = self
            .nodes
            .read()
            .get(&dir_id)
            .cloned()
            .ok_or_else(|| anyhow!("Directory inode {dir_id} not found"))?;

        if !dir_node.is_dir {
            return Err(anyhow!("Inode {dir_id} is not a directory"));
        }

        let oid = dir_node.oid.read().clone();
        let tree = self.git_engine.get_tree(&oid);

        let mut child_ids = Vec::new();
        let mut new_nodes = Vec::new();
        let mut new_child_mappings = Vec::new();

        if let Some(tree) = tree {
            for entry in &tree.entries {
                let existing_id = self.child_map.read().get(&(dir_id, entry.name.clone())).copied();
                let child_id = if let Some(id) = existing_id {
                    id
                } else {
                    let id = self.next_inode.fetch_add(1, Ordering::Relaxed);
                    let is_dir = entry.mode.is_dir();
                    let initial_size = if is_dir {
                        4096
                    } else if let Some(blob) = self.git_engine.memory_cache().get(&entry.oid) {
                        blob.len() as u64
                    } else if let Some(len) = self.git_engine.disk_cache().get_blob_size(&entry.oid) {
                        len
                    } else {
                        1024 * 1024
                    };

                    let node = Arc::new(VfsNode {
                        id,
                        parent_id: RwLock::new(dir_id),
                        name: RwLock::new(entry.name.clone()),
                        oid: RwLock::new(entry.oid.clone()),
                        mode: entry.mode.clone(),
                        is_dir,
                        size: RwLock::new(initial_size),
                        is_deleted: RwLock::new(false),
                    });

                    new_nodes.push((id, node));
                    new_child_mappings.push(((dir_id, entry.name.clone()), id));
                    id
                };

                child_ids.push(child_id);
            }
        }

        // Insert new nodes into state
        {
            let mut nodes = self.nodes.write();
            for (id, node) in new_nodes {
                nodes.insert(id, node);
            }
        }
        {
            let mut child_map = self.child_map.write();
            for (k, v) in new_child_mappings {
                child_map.insert(k, v);
            }
        }
        {
            let mut dir_children = self.dir_children.write();
            dir_children.insert(dir_id, child_ids);
        }

        Ok(())
    }

    /// Looks up a child name under a directory.
    pub fn lookup(&self, dir_id: u64, name: &str) -> Result<u64> {
        if name == "." {
            return Ok(dir_id);
        }
        if name == ".." {
            let node = self
                .nodes
                .read()
                .get(&dir_id)
                .cloned()
                .ok_or_else(|| anyhow!("Dir {dir_id} not found"))?;
            return Ok(*node.parent_id.read());
        }

        // Check child_map
        if let Some(&id) = self.child_map.read().get(&(dir_id, name.to_string())) {
            if let Some(node) = self.nodes.read().get(&id) {
                if !*node.is_deleted.read() {
                    return Ok(id);
                }
            }
        }

        // Populate directory if needed
        self.ensure_dir_populated(dir_id)?;

        if let Some(&id) = self.child_map.read().get(&(dir_id, name.to_string())) {
            if let Some(node) = self.nodes.read().get(&id) {
                if !*node.is_deleted.read() {
                    return Ok(id);
                }
            }
        }

        Err(anyhow!("Child '{name}' not found under dir {dir_id}"))
    }

    /// Lists directory entries as child nodes.
    pub fn list_dir(&self, dir_id: u64) -> Result<Vec<Arc<VfsNode>>> {
        self.ensure_dir_populated(dir_id)?;

        let child_ids = self
            .dir_children
            .read()
            .get(&dir_id)
            .cloned()
            .unwrap_or_default();

        let nodes = self.nodes.read();
        let mut list = Vec::with_capacity(child_ids.len());
        for id in child_ids {
            if let Some(node) = nodes.get(&id) {
                if !*node.is_deleted.read() {
                    list.push(node.clone());
                }
            }
        }

        Ok(list)
    }

    /// Creates a new regular file under `dir_id`.
    pub fn create_file(&self, dir_id: u64, name: &str, mode: TreeEntryMode) -> Result<Arc<VfsNode>> {
        self.ensure_dir_populated(dir_id)?;

        // If file exists and was deleted, revive it
        if let Some(&existing_id) = self.child_map.read().get(&(dir_id, name.to_string())) {
            if let Some(node) = self.nodes.read().get(&existing_id) {
                *node.is_deleted.write() = false;
                *node.size.write() = 0;
                return Ok(node.clone());
            }
        }

        let id = self.next_inode.fetch_add(1, Ordering::Relaxed);
        let node = Arc::new(VfsNode {
            id,
            parent_id: RwLock::new(dir_id),
            name: RwLock::new(name.to_string()),
            oid: RwLock::new(String::new()),
            mode,
            is_dir: false,
            size: RwLock::new(0),
            is_deleted: RwLock::new(false),
        });

        self.nodes.write().insert(id, node.clone());
        self.child_map.write().insert((dir_id, name.to_string()), id);
        self.dir_children.write().entry(dir_id).or_default().push(id);

        Ok(node)
    }

    /// Creates a new subdirectory under `dir_id`.
    pub fn mkdir(&self, dir_id: u64, name: &str) -> Result<Arc<VfsNode>> {
        self.ensure_dir_populated(dir_id)?;

        let id = self.next_inode.fetch_add(1, Ordering::Relaxed);
        let node = Arc::new(VfsNode {
            id,
            parent_id: RwLock::new(dir_id),
            name: RwLock::new(name.to_string()),
            oid: RwLock::new(String::new()),
            mode: TreeEntryMode::Directory,
            is_dir: true,
            size: RwLock::new(4096),
            is_deleted: RwLock::new(false),
        });

        self.nodes.write().insert(id, node.clone());
        self.child_map.write().insert((dir_id, name.to_string()), id);
        self.dir_children.write().entry(dir_id).or_default().push(id);
        // Initialize empty children list for this directory
        self.dir_children.write().insert(id, Vec::new());

        Ok(node)
    }

    /// Removes a file or directory under `dir_id`.
    pub fn remove(&self, dir_id: u64, name: &str) -> Result<Arc<VfsNode>> {
        self.ensure_dir_populated(dir_id)?;

        let id = self
            .child_map
            .read()
            .get(&(dir_id, name.to_string()))
            .copied()
            .ok_or_else(|| anyhow!("Child '{name}' not found under dir {dir_id}"))?;

        let node = self
            .nodes
            .read()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("Node {id} not found"))?;

        *node.is_deleted.write() = true;

        if let Some(children) = self.dir_children.write().get_mut(&dir_id) {
            children.retain(|&child_id| child_id != id);
        }
        self.child_map.write().remove(&(dir_id, name.to_string()));

        Ok(node)
    }

    /// Renames a file or directory.
    pub fn rename(
        &self,
        from_dir_id: u64,
        from_name: &str,
        to_dir_id: u64,
        to_name: &str,
    ) -> Result<()> {
        self.ensure_dir_populated(from_dir_id)?;
        self.ensure_dir_populated(to_dir_id)?;

        let id = self
            .child_map
            .read()
            .get(&(from_dir_id, from_name.to_string()))
            .copied()
            .ok_or_else(|| anyhow!("Source file '{from_name}' not found"))?;

        let node = self
            .nodes
            .read()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("Node {id} not found"))?;

        // If destination exists, remove it
        let _ = self.remove(to_dir_id, to_name);

        // Update node state
        *node.parent_id.write() = to_dir_id;
        *node.name.write() = to_name.to_string();

        // Update child mappings
        self.child_map.write().remove(&(from_dir_id, from_name.to_string()));
        self.child_map.write().insert((to_dir_id, to_name.to_string()), id);

        if let Some(children) = self.dir_children.write().get_mut(&from_dir_id) {
            children.retain(|&child_id| child_id != id);
        }
        self.dir_children.write().entry(to_dir_id).or_default().push(id);

        Ok(())
    }

    /// Updates real size of a node.
    pub fn update_file_size(&self, id: u64, real_size: u64) {
        if let Some(node) = self.nodes.read().get(&id) {
            *node.size.write() = real_size;
        }
    }

    /// Returns a list of all active nodes.
    #[allow(dead_code)]
    pub fn all_active_nodes(&self) -> Vec<Arc<VfsNode>> {
        self.nodes
            .read()
            .values()
            .filter(|n| !*n.is_deleted.read())
            .cloned()
            .collect()
    }

    /// Reconstructs the repository-relative path for a given node ID.
    /// Returns "" for the root inode.
    pub fn get_path(&self, node_id: u64) -> Result<String> {
        if node_id == ROOT_INODE {
            return Ok(String::new());
        }

        let mut components = Vec::new();
        let mut curr = node_id;

        while curr != ROOT_INODE {
            let node = self
                .nodes
                .read()
                .get(&curr)
                .cloned()
                .ok_or_else(|| anyhow!("Node {curr} not found in VFS"))?;

            let name = node.name.read().clone();
            components.push(name);

            let parent = *node.parent_id.read();
            if parent == curr {
                break;
            }
            curr = parent;
        }

        components.reverse();
        Ok(components.join("/"))
    }

    /// Resolves a repository-relative path (e.g. "foo/bar/baz.txt") to an inode ID.
    pub fn lookup_path(&self, path: &str) -> Result<u64> {
        let clean = path.trim_matches('/');
        if clean.is_empty() {
            return Ok(ROOT_INODE);
        }

        let mut curr = ROOT_INODE;
        for part in clean.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            curr = self.lookup(curr, part)?;
        }

        Ok(curr)
    }

    /// Resolves or creates all parent directories along a path, returning
    /// (parent_dir_id, basename).
    pub fn ensure_parent_dirs(&self, path: &str) -> Result<(u64, String)> {
        let clean = path.trim_matches('/');
        let parts: Vec<&str> = clean.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(anyhow!("Cannot resolve parent of empty path"));
        }

        let basename = parts[parts.len() - 1].to_string();
        let mut curr = ROOT_INODE;

        for &part in &parts[..parts.len() - 1] {
            match self.lookup(curr, part) {
                Ok(child_id) => {
                    curr = child_id;
                }
                Err(_) => {
                    let new_dir = self.mkdir(curr, part)?;
                    curr = new_dir.id;
                }
            }
        }

        Ok((curr, basename))
    }
}
