use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use crate::wal::{
    CreateFileMutation, InodeType, MkdirMutation, RemoveMutation, RenameMutation,
    TruncateMutation, WalManager, WalPayload, WriteMutation,
};

pub struct GitNfsFileSystem {
    vfs: Arc<VfsManager>,
    git_engine: Arc<GitEngine>,
    staging: Arc<StagingStore>,
    wal: Option<Arc<WalManager>>,
    uid: u32,
    gid: u32,
    epoch_seconds: u32,
    last_mutation_time: Arc<AtomicU64>,
    has_uncommitted_changes: Arc<AtomicBool>,
    commit_lock: Arc<tokio::sync::RwLock<()>>,
}

impl GitNfsFileSystem {
    pub fn new(
        vfs: Arc<VfsManager>,
        git_engine: Arc<GitEngine>,
        staging: Arc<StagingStore>,
        wal: Option<Arc<WalManager>>,
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
            wal,
            uid,
            gid,
            epoch_seconds,
            last_mutation_time: Arc::new(AtomicU64::new(0)),
            has_uncommitted_changes: Arc::new(AtomicBool::new(false)),
            commit_lock: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    pub fn commit_lock(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.commit_lock.clone()
    }

    pub fn last_mutation_time(&self) -> Arc<AtomicU64> {
        self.last_mutation_time.clone()
    }

    pub fn has_uncommitted_changes(&self) -> Arc<AtomicBool> {
        self.has_uncommitted_changes.clone()
    }

    fn record_mutation(&self, path: &str) {
        self.staging.record_changed_path(path);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_mutation_time.store(now, Ordering::Relaxed);
        self.has_uncommitted_changes.store(true, Ordering::Relaxed);
    }

    fn node_to_fattr3(&self, node: &VfsNode) -> fattr3 {
        let ftype = if node.is_dir {
            ftype3::NF3DIR
        } else if node.mode.is_symlink() {
            ftype3::NF3LNK
        } else {
            ftype3::NF3REG
        };

        // If staged, use staged size; if cached in memory/disk, use true blob length; otherwise use node.size
        let size = self
            .staging
            .get_size(node.id)
            .or_else(|| {
                let oid = node.oid.read();
                self.git_engine
                    .memory_cache()
                    .get(&oid)
                    .map(|d| d.len() as u64)
                    .or_else(|| self.git_engine.disk_cache().get_blob_size(&oid))
            })
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
        if !node.is_dir && !self.staging.is_staged(id) {
            let oid = node.oid.read().clone();
            if !oid.is_empty() {
                if let Some(blob) = self.git_engine.memory_cache().get(&oid) {
                    self.vfs.update_file_size(id, blob.len() as u64);
                } else if let Some(len) = self.git_engine.disk_cache().get_blob_size(&oid) {
                    self.vfs.update_file_size(id, len);
                } else if let Ok(data) = self.git_engine.get_blob_arc(&oid).await {
                    self.vfs.update_file_size(id, data.len() as u64);
                }
            }
        }
        Ok(self.node_to_fattr3(&node))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let _guard = self.commit_lock.read().await;
        if self.staging.is_deleted(id) {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        let node = self.vfs.get_node(id).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if let set_size3::size(new_size) = setattr.size {
            if node.is_dir {
                return Err(nfsstat3::NFS3ERR_ISDIR);
            }

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

            if let Ok(path) = self.vfs.get_path(id) {
                self.record_mutation(&path);
                if let Some(ref wal) = self.wal {
                    let _ = wal
                        .log_mutation(WalPayload::Truncate(TruncateMutation {
                            path,
                            new_size,
                        }))
                        .await;
                }
            }
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

        // 1. Check staging first
        if let Ok(Some((data, eof))) = self.staging.read_at(id, offset, count) {
            return Ok((data, eof));
        }

        // 2. Fetch from GitEngine (cache / remote)
        let oid = node.oid.read().clone();
        if oid.is_empty() {
            return Ok((Vec::new(), true));
        }

        // If the blob is missing from both memory and disk caches, prefetch
        // all companion files in the parent directory in a single batch request!
        if self.git_engine.memory_cache().get(&oid).is_none()
            && !self.git_engine.disk_cache().has_blob(&oid)
        {
            let parent_id = *node.parent_id.read();
            if let Some(parent_node) = self.vfs.get_node(parent_id) {
                let parent_oid = parent_node.oid.read().clone();
                let _ = self.git_engine.prefetch_directory_blobs(&parent_oid).await;
            }
        }

        let blob_data = self.git_engine.get_blob_arc(&oid).await.map_err(|e| {
            warn!("Failed to read blob {oid} for node {id}: {e}");
            nfsstat3::NFS3ERR_IO
        })?;

        let file_len = blob_data.len() as u64;
        self.vfs.update_file_size(id, file_len);

        if offset >= file_len {
            return Ok((Vec::new(), true));
        }

        let to_read = (count as u64).min(file_len - offset) as usize;
        let start = offset as usize;
        let slice = blob_data[start..start + to_read].to_vec();
        let eof = (offset + to_read as u64) >= file_len;

        Ok((slice, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let _guard = self.commit_lock.read().await;
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

        if let Ok(path) = self.vfs.get_path(id) {
            self.record_mutation(&path);
            if let Some(ref wal) = self.wal {
                let _ = wal
                    .log_mutation(WalPayload::Write(WriteMutation {
                        path,
                        offset,
                        data: data.to_vec(),
                    }))
                    .await;
            }
        }

        Ok(self.node_to_fattr3(&node))
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, _attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let _guard = self.commit_lock.read().await;
        let name = String::from_utf8_lossy(&filename.0).to_string();
        debug!("NFS CREATE: {name} in dir {dirid}");

        let node = self
            .vfs
            .create_file(dirid, &name, TreeEntryMode::RegularFile)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        self.staging.create_file(node.id).map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let parent_path = self.vfs.get_path(dirid).unwrap_or_default();
        let path = if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}/{name}")
        };
        self.record_mutation(&path);

        if let Some(ref wal) = self.wal {
            let _ = wal
                .log_mutation(WalPayload::CreateFile(CreateFileMutation {
                    path,
                    inode_type: InodeType::RegularFile as i32,
                    mode: 0o100644,
                }))
                .await;
        }

        let attr = self.node_to_fattr3(&node);
        Ok((node.id, attr))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let _guard = self.commit_lock.read().await;
        let name = String::from_utf8_lossy(&filename.0).to_string();
        debug!("NFS CREATE_EXCLUSIVE: {name} in dir {dirid}");

        let node = self
            .vfs
            .create_file(dirid, &name, TreeEntryMode::RegularFile)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        self.staging.create_file(node.id).map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let parent_path = self.vfs.get_path(dirid).unwrap_or_default();
        let path = if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}/{name}")
        };
        self.record_mutation(&path);

        if let Some(ref wal) = self.wal {
            let _ = wal
                .log_mutation(WalPayload::CreateFile(CreateFileMutation {
                    path,
                    inode_type: InodeType::RegularFile as i32,
                    mode: 0o100644,
                }))
                .await;
        }

        Ok(node.id)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let _guard = self.commit_lock.read().await;
        let name = String::from_utf8_lossy(&dirname.0).to_string();
        debug!("NFS MKDIR: {name} in dir {dirid}");

        let node = self
            .vfs
            .mkdir(dirid, &name)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        let parent_path = self.vfs.get_path(dirid).unwrap_or_default();
        let path = if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}/{name}")
        };
        self.record_mutation(&path);

        if let Some(ref wal) = self.wal {
            let _ = wal
                .log_mutation(WalPayload::Mkdir(MkdirMutation { path }))
                .await;
        }

        let attr = self.node_to_fattr3(&node);
        Ok((node.id, attr))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let _guard = self.commit_lock.read().await;
        let name = String::from_utf8_lossy(&filename.0).to_string();
        debug!("NFS REMOVE: {name} in dir {dirid}");

        let node = self
            .vfs
            .remove(dirid, &name)
            .map_err(|_| nfsstat3::NFS3ERR_NOENT)?;

        self.staging.mark_deleted(node.id);

        let parent_path = self.vfs.get_path(dirid).unwrap_or_default();
        let path = if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}/{name}")
        };
        self.record_mutation(&path);

        if let Some(ref wal) = self.wal {
            let _ = wal
                .log_mutation(WalPayload::Remove(RemoveMutation { path }))
                .await;
        }

        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        let _guard = self.commit_lock.read().await;
        let from_name = String::from_utf8_lossy(&from_filename.0).to_string();
        let to_name = String::from_utf8_lossy(&to_filename.0).to_string();
        debug!("NFS RENAME: {from_name} -> {to_name}");

        let from_parent = self.vfs.get_path(from_dirid).unwrap_or_default();
        let from_path = if from_parent.is_empty() {
            from_name.clone()
        } else {
            format!("{from_parent}/{from_name}")
        };
        let to_parent = self.vfs.get_path(to_dirid).unwrap_or_default();
        let to_path = if to_parent.is_empty() {
            to_name.clone()
        } else {
            format!("{to_parent}/{to_name}")
        };

        self.vfs
            .rename(from_dirid, &from_name, to_dirid, &to_name)
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;

        self.record_mutation(&from_path);
        self.record_mutation(&to_path);

        if let Some(ref wal) = self.wal {
            let _ = wal
                .log_mutation(WalPayload::Rename(RenameMutation {
                    from_path,
                    to_path,
                }))
                .await;
        }

        Ok(())
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

        // On first readdir page of a directory, trigger background prefetch of its file blobs
        if start_after == 0 {
            let dir_oid = dir_node.oid.read().clone();
            let engine = self.git_engine.clone();
            tokio::spawn(async move {
                let _ = engine.prefetch_directory_blobs(&dir_oid).await;
            });
        }

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
        let data = self.git_engine.get_blob_arc(&oid).await.map_err(|e| {
            warn!("Failed to read symlink target {}: {e}", oid);
            nfsstat3::NFS3ERR_IO
        })?;

        Ok(nfspath3::from((*data).clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_setattr_directory_permits_non_size_attrs() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let git_engine = Arc::new(GitEngine::new("https://example.com/repo.git", Some(temp.path()))?);
        let staging = Arc::new(StagingStore::new(temp.path())?);
        let vfs = Arc::new(VfsManager::new(git_engine.clone(), "0000000000000000000000000000000000000000"));
        let nfs = GitNfsFileSystem::new(vfs.clone(), git_engine, staging, None);

        // 1. Create a directory via NFS
        let dir_name = nfsstring(b"test_dir".to_vec());
        let (dir_id, attr) = nfs.mkdir(ROOT_INODE, &dir_name).await
            .expect("mkdir must succeed");
        assert!(matches!(attr.ftype, ftype3::NF3DIR));

        // 2. Setting timestamps/mode with size=Void must succeed
        let sattr = sattr3 {
            mode: set_mode3::mode(0o755),
            uid: set_uid3::Void,
            gid: set_gid3::Void,
            size: set_size3::Void,
            atime: set_atime::SET_TO_SERVER_TIME,
            mtime: set_mtime::SET_TO_SERVER_TIME,
        };
        let res = nfs.setattr(dir_id, sattr).await;
        assert!(res.is_ok(), "setattr with non-size attributes on directory should succeed, got {res:?}");
        let new_attr = res.unwrap();
        assert!(matches!(new_attr.ftype, ftype3::NF3DIR));

        // 3. Attempting to set size on a directory must fail with NFS3ERR_ISDIR
        let sattr_with_size = sattr3 {
            mode: set_mode3::Void,
            uid: set_uid3::Void,
            gid: set_gid3::Void,
            size: set_size3::size(100),
            atime: set_atime::DONT_CHANGE,
            mtime: set_mtime::DONT_CHANGE,
        };
        let err = nfs.setattr(dir_id, sattr_with_size).await
            .expect_err("resizing directory must fail with NFS3ERR_ISDIR");
        assert!(matches!(err, nfsstat3::NFS3ERR_ISDIR));

        Ok(())
    }

    #[tokio::test]
    async fn test_setattr_file_allows_truncation() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let git_engine = Arc::new(GitEngine::new("https://example.com/repo.git", Some(temp.path()))?);
        let staging = Arc::new(StagingStore::new(temp.path())?);
        let vfs = Arc::new(VfsManager::new(git_engine.clone(), "0000000000000000000000000000000000000000"));
        let nfs = GitNfsFileSystem::new(vfs.clone(), git_engine, staging, None);

        // Create a file and write data
        let file_name = nfsstring(b"file.txt".to_vec());
        let (file_id, _) = nfs.create(ROOT_INODE, &file_name, sattr3::default()).await
            .expect("create file must succeed");
        nfs.write(file_id, 0, b"hello world").await
            .expect("write must succeed");

        let attr = nfs.getattr(file_id).await.expect("getattr must succeed");
        assert_eq!(attr.size, 11);

        // Truncate file via setattr
        let sattr = sattr3 {
            mode: set_mode3::Void,
            uid: set_uid3::Void,
            gid: set_gid3::Void,
            size: set_size3::size(5),
            atime: set_atime::DONT_CHANGE,
            mtime: set_mtime::DONT_CHANGE,
        };
        let res = nfs.setattr(file_id, sattr).await.expect("setattr on file must succeed");
        assert_eq!(res.size, 5);

        let (read_data, _) = nfs.read(file_id, 0, 100).await.expect("read must succeed");
        assert_eq!(read_data, b"hello");

        Ok(())
    }
}

