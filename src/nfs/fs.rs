use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use nfsserve::nfs::*;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tracing::{debug, warn};

use crate::git::tree::TreeEntryMode;
use crate::git::GitEngine;
use crate::staging::StagingStore;
use crate::vfs::inode::{VfsManager, VfsNode, ROOT_INODE};

pub struct GitNfsFileSystem {
    vfs: Arc<VfsManager>,
    git_engine: Arc<GitEngine>,
    staging: Arc<StagingStore>,
    uid: u32,
    gid: u32,
    epoch_seconds: u32,
}

impl GitNfsFileSystem {
    pub fn new(
        vfs: Arc<VfsManager>,
        git_engine: Arc<GitEngine>,
        staging: Arc<StagingStore>,
    ) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let epoch_seconds = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        Self {
            vfs,
            git_engine,
            staging,
            uid,
            gid,
            epoch_seconds,
        }
    }

    fn node_to_fattr3(&self, node: &VfsNode) -> fattr3 {
        let ftype = if node.is_dir {
            ftype3::NF3DIR
        } else if node.mode.is_symlink() {
            ftype3::NF3LNK
        } else {
            ftype3::NF3REG
        };

        // If staged, use staged size; otherwise use cached size
        let size = self
            .staging
            .get_size(node.id)
            .unwrap_or_else(|| *node.size.read());

        fattr3 {
            ftype,
            mode: node.mode.posix_mode() & 0o777,
            nlink: if node.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: node.id,
            atime: nfstime3 {
                seconds: self.epoch_seconds,
                nseconds: 0,
            },
            mtime: nfstime3 {
                seconds: self.epoch_seconds,
                nseconds: 0,
            },
            ctime: nfstime3 {
                seconds: self.epoch_seconds,
                nseconds: 0,
            },
        }
    }

    /// Fetches the base blob for a node if needed before applying staging writes.
    async fn get_base_data(&self, node: &VfsNode) -> Option<Vec<u8>> {
        let oid = node.oid.read().clone();
        if oid.is_empty() {
            return None;
        }
        self.git_engine.get_blob(&oid).await.ok()
    }
}

#[async_trait]
impl NFSFileSystem for GitNfsFileSystem {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_INODE
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = String::from_utf8_lossy(&filename.0);

        match self.vfs.lookup(dirid, &name) {
            Ok(child_id) => {
                if self.staging.is_deleted(child_id) {
                    Err(nfsstat3::NFS3ERR_NOENT)
                } else {
                    Ok(child_id)
                }
            }
            Err(_) => Err(nfsstat3::NFS3ERR_NOENT),
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        if self.staging.is_deleted(id) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let node = self.vfs.get_node(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(self.node_to_fattr3(&node))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        if self.staging.is_deleted(id) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let node = self.vfs.get_node(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if node.is_dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }

        if let set_size3::size(new_size) = setattr.size {
            let base_data = if !self.staging.is_staged(id) {
                self.get_base_data(&node).await
            } else {
                None
            };

            self.staging
                .truncate(id, new_size, base_data.as_deref())
                .map_err(|e| {
                    warn!("Failed to truncate node {id}: {e}");
                    nfsstat3::NFS3ERR_IO
                })?;
            self.vfs.update_file_size(id, new_size);
        }

        Ok(self.node_to_fattr3(&node))
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        if self.staging.is_deleted(id) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let node = self.vfs.get_node(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if node.is_dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }

        // 1. Check if file has staged modifications
        if let Some((buf, eof)) = self.staging.read_at(id, offset, count).map_err(|e| {
            warn!("Failed to read from staging for node {id}: {e}");
            nfsstat3::NFS3ERR_IO
        })? {
            return Ok((buf, eof));
        }

        // 2. Otherwise read from original Git blob
        let oid = node.oid.read().clone();
        let data: Vec<u8> = self.git_engine.get_blob(&oid).await.map_err(|e| {
            warn!("Failed to fetch blob {}: {e}", oid);
            nfsstat3::NFS3ERR_IO
        })?;

        let real_size = data.len() as u64;
        self.vfs.update_file_size(id, real_size);

        if offset >= real_size {
            return Ok((Vec::new(), true));
        }

        let start = offset as usize;
        let end = (start + count as usize).min(data.len());
        let slice = data[start..end].to_vec();
        let eof = end >= data.len();

        Ok((slice, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        if self.staging.is_deleted(id) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let node = self.vfs.get_node(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if node.is_dir {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }

        let base_data = if !self.staging.is_staged(id) {
            self.get_base_data(&node).await
        } else {
            None
        };

        let new_len = self
            .staging
            .write_at(id, offset, data, base_data.as_deref())
            .map_err(|e| {
                warn!("Failed to write to staging for node {id}: {e}");
                nfsstat3::NFS3ERR_IO
            })?;

        self.vfs.update_file_size(id, new_len);
        Ok(self.node_to_fattr3(&node))
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, _attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = String::from_utf8_lossy(&filename.0).to_string();
        debug!("NFS CREATE: {name} in dir {dirid}");

        let node = self
            .vfs
            .create_file(dirid, &name, TreeEntryMode::RegularFile)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        self.staging.create_file(node.id).map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let attr = self.node_to_fattr3(&node);
        Ok((node.id, attr))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = String::from_utf8_lossy(&filename.0).to_string();
        debug!("NFS CREATE_EXCLUSIVE: {name} in dir {dirid}");

        let node = self
            .vfs
            .create_file(dirid, &name, TreeEntryMode::RegularFile)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        self.staging.create_file(node.id).map_err(|_| nfsstat3::NFS3ERR_IO)?;

        Ok(node.id)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let name = String::from_utf8_lossy(&dirname.0).to_string();
        debug!("NFS MKDIR: {name} in dir {dirid}");

        let node = self
            .vfs
            .mkdir(dirid, &name)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let attr = self.node_to_fattr3(&node);
        Ok((node.id, attr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = String::from_utf8_lossy(&filename.0).to_string();
        debug!("NFS REMOVE: {name} in dir {dirid}");

        let node = self
            .vfs
            .remove(dirid, &name)
            .map_err(|_| nfsstat3::NFS3ERR_NOENT)?;

        self.staging.mark_deleted(node.id);
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let from_name = String::from_utf8_lossy(&from_filename.0).to_string();
        let to_name = String::from_utf8_lossy(&to_filename.0).to_string();
        debug!("NFS RENAME: {from_name} -> {to_name}");

        self.vfs
            .rename(from_dirid, &from_name, to_dirid, &to_name)
            .map_err(|_| nfsstat3::NFS3ERR_IO)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir_node = self.vfs.get_node(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if !dir_node.is_dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }

        let children = self.vfs.list_dir(dirid).map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let mut all_entries = Vec::with_capacity(children.len() + 2);

        // "."
        all_entries.push(DirEntry {
            fileid: dirid,
            name: nfsstring::from(b"."[..].to_vec()),
            attr: self.node_to_fattr3(&dir_node),
        });

        // ".."
        let parent_id = *dir_node.parent_id.read();
        let parent_node = self
            .vfs
            .get_node(parent_id)
            .unwrap_or_else(|| dir_node.clone());
        all_entries.push(DirEntry {
            fileid: parent_id,
            name: nfsstring::from(b"..\0"[..2].to_vec()),
            attr: self.node_to_fattr3(&parent_node),
        });

        // Children (filter out deleted or staged deleted)
        for child in children {
            if self.staging.is_deleted(child.id) {
                continue;
            }
            let child_name = child.name.read().clone();
            all_entries.push(DirEntry {
                fileid: child.id,
                name: nfsstring::from(child_name.as_bytes().to_vec()),
                attr: self.node_to_fattr3(&child),
            });
        }

        // Determine slice based on start_after
        let start_idx = if start_after == 0 {
            0
        } else {
            all_entries
                .iter()
                .position(|e| e.fileid == start_after)
                .map(|idx| idx + 1)
                .unwrap_or(0)
        };

        let end_idx = (start_idx + max_entries).min(all_entries.len());
        let end = end_idx >= all_entries.len();
        let entries: Vec<DirEntry> = all_entries.drain(..end_idx).skip(start_idx).collect();

        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let node = self.vfs.get_node(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if !node.mode.is_symlink() {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }

        let oid = node.oid.read().clone();
        let data: Vec<u8> = self.git_engine.get_blob(&oid).await.map_err(|e| {
            warn!("Failed to read symlink target {}: {e}", oid);
            nfsstat3::NFS3ERR_IO
        })?;

        Ok(nfspath3::from(data))
    }
}
