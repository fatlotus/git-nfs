use anyhow::{anyhow, Context, Result};
use tracing::{debug, info};

use crate::git::builder::{rebuild_git_objects, GitObjectToPack};
use crate::git::pack_writer::PackWriter;
use crate::git::tree::TreeEntryMode;
use crate::git::GitEngine;
use crate::staging::StagingStore;
use crate::vfs::inode::VfsManager;
use crate::wal::proto::{InodeType, WalEntry, WalPayload};

pub struct RecoveredPackResult {
    pub commit_oid: String,
    pub root_tree_oid: String,
    pub pack_data: Vec<u8>,
    pub idx_data: Vec<u8>,
    pub pack_sha: String,
    pub objects: Vec<GitObjectToPack>,
}

/// Replays a sequence of WAL entries into VfsManager and StagingStore.
pub async fn replay_wal_entries(
    entries: &[WalEntry],
    vfs: &VfsManager,
    staging: &StagingStore,
    git_engine: &GitEngine,
) -> Result<()> {
    for entry in entries {
        let payload = match &entry.payload {
            Some(p) => p,
            None => continue,
        };

        match payload {
            WalPayload::Header(_) => {
                // Header is parsed by caller for metadata (base commit, author, etc.)
            }
            WalPayload::CreateFile(create) => {
                debug!("Replay CREATE_FILE: {}", create.path);
                staging.record_changed_path(&create.path);
                let (parent_id, name) = vfs.ensure_parent_dirs(&create.path)?;
                let mode = match create.inode_type() {
                    InodeType::ExecutableFile => TreeEntryMode::ExecutableFile,
                    InodeType::Symlink => TreeEntryMode::Symlink,
                    _ => TreeEntryMode::RegularFile,
                };
                let node = vfs.create_file(parent_id, &name, mode)?;
                staging.create_file(node.id)?;
            }
            WalPayload::Mkdir(mkdir) => {
                debug!("Replay MKDIR: {}", mkdir.path);
                staging.record_changed_path(&mkdir.path);
                let (parent_id, name) = vfs.ensure_parent_dirs(&mkdir.path)?;
                vfs.mkdir(parent_id, &name)?;
            }
            WalPayload::Write(write) => {
                debug!(
                    "Replay WRITE: {} (offset: {}, len: {})",
                    write.path,
                    write.offset,
                    write.data.len()
                );
                staging.record_changed_path(&write.path);
                let node_id = match vfs.lookup_path(&write.path) {
                    Ok(id) => id,
                    Err(_) => {
                        let (parent_id, name) = vfs.ensure_parent_dirs(&write.path)?;
                        let node = vfs.create_file(parent_id, &name, TreeEntryMode::RegularFile)?;
                        staging.create_file(node.id)?;
                        node.id
                    }
                };

                let node = vfs
                    .get_node(node_id)
                    .ok_or_else(|| anyhow!("Node {node_id} not found in VFS during write replay"))?;

                let base_data = if !staging.is_staged(node_id) {
                    let oid = node.oid.read().clone();
                    if !oid.is_empty() {
                        git_engine.get_blob(&oid).await.ok()
                    } else {
                        None
                    }
                } else {
                    None
                };

                let new_len = staging.write_at(
                    node_id,
                    write.offset,
                    &write.data,
                    base_data.as_deref(),
                )?;
                vfs.update_file_size(node_id, new_len);
            }
            WalPayload::Truncate(trunc) => {
                debug!("Replay TRUNCATE: {} to {} bytes", trunc.path, trunc.new_size);
                staging.record_changed_path(&trunc.path);
                if let Ok(node_id) = vfs.lookup_path(&trunc.path) {
                    let node = vfs
                        .get_node(node_id)
                        .ok_or_else(|| anyhow!("Node {node_id} not found during truncate replay"))?;

                    let base_data = if !staging.is_staged(node_id) {
                        let oid = node.oid.read().clone();
                        if !oid.is_empty() {
                            git_engine.get_blob(&oid).await.ok()
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    staging.truncate(node_id, trunc.new_size, base_data.as_deref())?;
                    vfs.update_file_size(node_id, trunc.new_size);
                }
            }
            WalPayload::Remove(remove) => {
                debug!("Replay REMOVE: {}", remove.path);
                staging.record_changed_path(&remove.path);
                if let Ok(node_id) = vfs.lookup_path(&remove.path) {
                    if let Ok((parent_id, name)) = vfs.ensure_parent_dirs(&remove.path) {
                        let _ = vfs.remove(parent_id, &name);
                    }
                    staging.mark_deleted(node_id);
                }
            }
            WalPayload::Rename(rename) => {
                debug!("Replay RENAME: {} -> {}", rename.from_path, rename.to_path);
                staging.record_changed_path(&rename.from_path);
                staging.record_changed_path(&rename.to_path);
                if let Ok((from_parent, from_name)) = vfs.ensure_parent_dirs(&rename.from_path) {
                    if let Ok((to_parent, to_name)) = vfs.ensure_parent_dirs(&rename.to_path) {
                        let _ = vfs.rename(from_parent, &from_name, to_parent, &to_name);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Generates a Git commit, packfile, and index from the current reconstructed staging state.
pub async fn build_pack_from_recovered_state(
    vfs: &VfsManager,
    staging: &StagingStore,
    git_engine: &GitEngine,
    base_commit_oid: &str,
    author: &str,
    commit_message: &str,
) -> Result<Option<RecoveredPackResult>> {
    if !staging.has_modifications() {
        return Ok(None);
    }

    let (commit_oid, root_tree_oid, objects) = rebuild_git_objects(
        vfs,
        staging,
        git_engine,
        base_commit_oid,
        author,
        commit_message,
    )
    .await
    .context("Rebuilding Git tree objects during WAL recovery")?;

    let (pack_data, idx_data, pack_sha) = PackWriter::create_pack_and_index_bytes(&objects)
        .context("Generating pack and index bytes from recovered objects")?;

    info!(
        "Successfully reconstructed Git pack (pack-{}) from WAL: commit {} (tree {}) with {} objects ({} bytes pack, {} bytes idx)",
        pack_sha,
        commit_oid,
        root_tree_oid,
        objects.len(),
        pack_data.len(),
        idx_data.len()
    );

    Ok(Some(RecoveredPackResult {
        commit_oid,
        root_tree_oid,
        pack_data,
        idx_data,
        pack_sha,
        objects,
    }))
}
