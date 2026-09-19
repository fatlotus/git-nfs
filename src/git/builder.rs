use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use tracing::info;

use crate::git::pack::compute_git_sha1;
use crate::git::pack::ObjectType;
use crate::git::tree::TreeEntryMode;
use crate::git::GitEngine;
use crate::staging::StagingStore;
use crate::vfs::filter::is_apple_metadata;
use crate::vfs::inode::{VfsManager, ROOT_INODE};

#[derive(Debug, Clone)]
pub struct GitObjectToPack {
    pub oid: String,
    pub obj_type: ObjectType,
    pub data: Vec<u8>,
}

/// Sorts two git tree entry names according to Git's canonical sorting rule
/// (where directories are sorted as if their names have a trailing slash '/').
pub fn compare_tree_entries(name_a: &str, is_dir_a: bool, name_b: &str, is_dir_b: bool) -> Ordering {
    let mut a = name_a.as_bytes().to_vec();
    if is_dir_a {
        a.push(b'/');
    }
    let mut b = name_b.as_bytes().to_vec();
    if is_dir_b {
        b.push(b'/');
    }
    a.cmp(&b)
}

/// Recursively reconstructs modified Git trees starting from root.
/// Returns all newly created objects (blobs, trees, commit) ready for packing.
pub async fn rebuild_git_objects(
    vfs: &VfsManager,
    staging: &StagingStore,
    _git_engine: &GitEngine,
    base_commit_oid: &str,
    author: &str,
    commit_message: &str,
) -> Result<(String, Vec<GitObjectToPack>)> {
    let mut new_objects: HashMap<String, GitObjectToPack> = HashMap::new();

    // 1. Process all staged (modified or created) files into new Git blobs
    let staged_node_ids = staging.list_staged_nodes();
    for node_id in staged_node_ids {
        if let Some(node) = vfs.get_node(node_id) {
            let name = node.name.read().clone();
            // Skip macOS metadata files from git objects
            if is_apple_metadata(&name) {
                continue;
            }

            if let Some(content) = staging.get_staged_content(node_id)? {
                let sha = compute_git_sha1(ObjectType::Blob, &content);
                *node.oid.write() = sha.clone();

                new_objects.insert(
                    sha.clone(),
                    GitObjectToPack {
                        oid: sha,
                        obj_type: ObjectType::Blob,
                        data: content,
                    },
                );
            }
        }
    }

    // 2. Recursively rebuild trees starting from root inode (1)
    let new_root_tree_oid = rebuild_tree_recursive(ROOT_INODE, vfs, &mut new_objects)?;

    // 3. Construct new Commit Object
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let commit_content = format!(
        "tree {}\nparent {}\nauthor {} {} +0000\ncommitter {} {} +0000\n\n{}\n",
        new_root_tree_oid, base_commit_oid, author, timestamp, author, timestamp, commit_message
    );
    let commit_bytes = commit_content.into_bytes();
    let new_commit_oid = compute_git_sha1(ObjectType::Commit, &commit_bytes);

    new_objects.insert(
        new_commit_oid.clone(),
        GitObjectToPack {
            oid: new_commit_oid.clone(),
            obj_type: ObjectType::Commit,
            data: commit_bytes,
        },
    );

    let objects_list: Vec<GitObjectToPack> = new_objects.into_values().collect();
    info!(
        "Rebuilt Git state: New commit {} with {} objects",
        new_commit_oid,
        objects_list.len()
    );

    Ok((new_commit_oid, objects_list))
}

fn rebuild_tree_recursive(
    dir_id: u64,
    vfs: &VfsManager,
    new_objects: &mut HashMap<String, GitObjectToPack>,
) -> Result<String> {
    let children = vfs.list_dir(dir_id)?;

    // Filter out Apple metadata and sort children canonically
    let mut valid_children: Vec<_> = children
        .into_iter()
        .filter(|c| !is_apple_metadata(&c.name.read()))
        .collect();

    valid_children.sort_by(|a, b| {
        compare_tree_entries(
            &a.name.read(),
            a.is_dir,
            &b.name.read(),
            b.is_dir,
        )
    });

    let mut raw_tree = Vec::new();

    for child in valid_children {
        let child_name = child.name.read().clone();
        let child_oid = if child.is_dir {
            // Recurse into subdirectory to get its (potentially new) tree OID
            rebuild_tree_recursive(child.id, vfs, new_objects)?
        } else {
            child.oid.read().clone()
        };

        if child_oid.is_empty() {
            continue;
        }

        let mode_str = match child.mode {
            TreeEntryMode::RegularFile => "100644",
            TreeEntryMode::ExecutableFile => "100755",
            TreeEntryMode::Directory => "40000",
            TreeEntryMode::Symlink => "120000",
            TreeEntryMode::Submodule => "160000",
            TreeEntryMode::Other(m) => &format!("{m:o}"),
        };

        // Format: "<mode> <name>\0<20-byte-raw-sha>"
        raw_tree.extend_from_slice(format!("{mode_str} {child_name}\0").as_bytes());
        let binary_sha = hex::decode(&child_oid)
            .map_err(|e| anyhow!("Invalid hex SHA {child_oid}: {e}"))?;
        raw_tree.extend_from_slice(&binary_sha);
    }

    let tree_sha = compute_git_sha1(ObjectType::Tree, &raw_tree);

    new_objects.insert(
        tree_sha.clone(),
        GitObjectToPack {
            oid: tree_sha.clone(),
            obj_type: ObjectType::Tree,
            data: raw_tree,
        },
    );

    Ok(tree_sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical_tree_sorting() {
        // "foo.c" vs "foo/" (directory): "." (46) < "/" (47), so "foo.c" comes BEFORE directory "foo"
        assert_eq!(
            compare_tree_entries("foo.c", false, "foo", true),
            Ordering::Less
        );

        // "foo/" (dir) vs "foo1" (file): "/" (47) < "1" (49), so directory "foo" comes BEFORE file "foo1"
        assert_eq!(
            compare_tree_entries("foo", true, "foo1", false),
            Ordering::Less
        );
    }
}
