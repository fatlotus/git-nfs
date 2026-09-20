use std::sync::Arc;
use tempfile::TempDir;

use git_nfs::git::GitEngine;
use git_nfs::staging::StagingStore;
use git_nfs::vfs::inode::VfsManager;
use git_nfs::wal::backend::WalBackend;
use git_nfs::wal::local::LocalWalBackend;
use git_nfs::wal::proto::{
    CreateFileMutation, InodeType, MkdirMutation, RemoveMutation, RenameMutation, TruncateMutation,
    WalEntry, WalHeader, WalPayload, WriteMutation,
};
use git_nfs::wal::recovery::{build_pack_from_recovered_state, replay_wal_entries};
use git_nfs::wal::WalManager;

#[tokio::test]
async fn test_decode_user_log() {
    let path = std::path::PathBuf::from("/Users/jeremyarcher/.cache/git-nfs/aa24f7914cebcc04de6104c814c089815e172c16/wal/4.log");
    if let Ok(bytes) = std::fs::read(&path) {
        let (entries, consumed) = WalEntry::decode_all_framed(&bytes);
        println!("User 4.log: read {} bytes, consumed {}, {} entries:", bytes.len(), consumed, entries.len());
        for (i, entry) in entries.iter().enumerate() {
            match &entry.payload {
                Some(WalPayload::Header(h)) => println!("  [{i}] Header: seq={}, base={}, author={}", h.wal_seq, h.base_commit_oid, h.author),
                Some(WalPayload::CreateFile(c)) => println!("  [{i}] CreateFile: path={}", c.path),
                Some(WalPayload::Mkdir(m)) => println!("  [{i}] Mkdir: path={}", m.path),
                Some(WalPayload::Write(w)) => println!("  [{i}] Write: path={}, offset={}, len={}", w.path, w.offset, w.data.len()),
                Some(WalPayload::Truncate(t)) => println!("  [{i}] Truncate: path={}, size={}", t.path, t.new_size),
                Some(WalPayload::Remove(r)) => println!("  [{i}] Remove: path={}", r.path),
                Some(WalPayload::Rename(rn)) => println!("  [{i}] Rename: {} -> {}", rn.from_path, rn.to_path),
                None => println!("  [{i}] None"),
            }
        }

        let temp = TempDir::new().unwrap();
        let cache_dir = std::path::PathBuf::from("/Users/jeremyarcher/.cache/git-nfs/aa24f7914cebcc04de6104c814c089815e172c16");
        let git_engine = Arc::new(GitEngine::new("https://github.com/torvalds/linux.git", Some(&cache_dir)).unwrap());
        let root_oid = git_engine.initialize(None).await.unwrap();
        let base_commit_oid = git_engine.base_commit_oid().unwrap();

        let vfs = Arc::new(VfsManager::new(git_engine.clone(), &root_oid));
        let staging = Arc::new(StagingStore::new(&temp.path().join("staging")).unwrap());

        replay_wal_entries(&entries, &vfs, &staging, &git_engine).await.unwrap();

        println!("After replaying 4.log: staging has modifications = {}", staging.has_modifications());
        println!("Staged nodes: {:?}", staging.list_staged_nodes());
        println!("Deleted nodes: {:?}", staging.list_deleted_nodes());

        for &node_id in &staging.list_staged_nodes() {
            if let Some(node) = vfs.get_node(node_id) {
                println!("Staged node {}: name={}, path={:?}", node_id, node.name.read(), vfs.get_path(node_id));
            } else {
                println!("Staged node {}: NOT IN VFS!", node_id);
            }
        }

        let pack_res = build_pack_from_recovered_state(
            &vfs,
            &staging,
            &git_engine,
            &base_commit_oid,
            "Jeremy Archer <open-source@fatlotus.com>",
            "test",
        ).await.unwrap();

        println!("Pack result is_some: {}", pack_res.is_some());
        if let Some(res) = pack_res {
            println!("Commit OID: {}", res.commit_oid);
            println!("Objects count: {}", res.objects.len());
            for obj in &res.objects {
                println!("  Obj: oid={}, type={:?}, size={}", obj.oid, obj.obj_type, obj.data.len());
            }
        }
    }
}

#[tokio::test]
async fn test_wal_entry_framing_and_crash_truncation() {
    let mut all_bytes = Vec::new();

    // 1. Valid Header Entry
    let entry1 = WalEntry {
        seq: 1,
        timestamp_unix_nanos: 1000,
        payload: Some(WalPayload::Header(WalHeader {
            repo_url: "https://example.com/repo.git".to_string(),
            base_commit_oid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            author: "Tester <test@example.com>".to_string(),
            commit_message: "Crash test".to_string(),
            started_at_unix_secs: 123456,
            wal_seq: 0,
        })),
    };
    all_bytes.extend_from_slice(&entry1.encode_framed());

    // 2. Valid CreateFile Entry
    let entry2 = WalEntry {
        seq: 2,
        timestamp_unix_nanos: 2000,
        payload: Some(WalPayload::CreateFile(CreateFileMutation {
            path: "test.txt".to_string(),
            inode_type: InodeType::RegularFile as i32,
            mode: 0o100644,
        })),
    };
    all_bytes.extend_from_slice(&entry2.encode_framed());

    // 3. Valid Write Entry
    let entry3 = WalEntry {
        seq: 3,
        timestamp_unix_nanos: 3000,
        payload: Some(WalPayload::Write(WriteMutation {
            path: "test.txt".to_string(),
            offset: 0,
            data: b"hello crash recovery".to_vec(),
        })),
    };
    all_bytes.extend_from_slice(&entry3.encode_framed());

    // 4. Simulate crash midway through appending entry 4:
    // Write a length prefix indicating 100 bytes, but only supply 10 bytes!
    all_bytes.extend_from_slice(&(100u32).to_be_bytes());
    all_bytes.extend_from_slice(&[0xaa; 10]);

    // Decode: should cleanly parse the 3 valid entries and safely ignore the truncated 4th
    let (decoded, consumed) = WalEntry::decode_all_framed(&all_bytes);
    assert_eq!(decoded.len(), 3);
    assert_eq!(decoded[0].seq, 1);
    assert_eq!(decoded[1].seq, 2);
    assert_eq!(decoded[2].seq, 3);
    assert!(consumed < all_bytes.len());
}

#[tokio::test]
async fn test_local_wal_backend_lifecycle_and_zombie_lockout() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let backend = LocalWalBackend::new(temp.path())?;

    assert_eq!(backend.list_log_sequences().await?, Vec::<u64>::new());

    // Write 0.log
    let mut writer0 = backend.open_writer(0).await?;
    let entry = WalEntry {
        seq: 1,
        timestamp_unix_nanos: 1,
        payload: Some(WalPayload::Mkdir(MkdirMutation {
            path: "dir1".to_string(),
        })),
    };
    writer0.append_entry(&entry).await?;
    writer0.flush().await?;
    Box::new(writer0).finalize().await?;

    // Write 1.log
    let mut writer1 = backend.open_writer(1).await?;
    let entry2 = WalEntry {
        seq: 2,
        timestamp_unix_nanos: 2,
        payload: Some(WalPayload::Mkdir(MkdirMutation {
            path: "dir2".to_string(),
        })),
    };
    writer1.append_entry(&entry2).await?;
    writer1.flush().await?;
    Box::new(writer1).finalize().await?;

    // Verify sequences
    let seqs = backend.list_log_sequences().await?;
    assert_eq!(seqs, vec![0, 1]);

    // Lockout zombies
    backend.lockout_zombies(&seqs).await?;

    // Neither is marked done yet
    assert!(!backend.is_log_done(0).await?);
    assert!(!backend.is_log_done(1).await?);

    // Mark 0 done
    backend
        .mark_log_done(0, "commit0", Some(b"pack0"), Some(b"idx0"))
        .await?;
    assert!(backend.is_log_done(0).await?);
    assert!(!backend.is_log_done(1).await?);

    // Verify reading log
    let bytes = backend.read_log(0).await?;
    let (entries, _) = WalEntry::decode_all_framed(&bytes);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seq, 1);

    Ok(())
}

#[tokio::test]
async fn test_wal_mutation_replay_and_packfile_generation() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let git_engine = Arc::new(GitEngine::new("https://example.com/repo.git", Some(temp.path()))?);
    let base_tree = "0000000000000000000000000000000000000000";
    let base_commit = "1111111111111111111111111111111111111111";

    let vfs = Arc::new(VfsManager::new(git_engine.clone(), base_tree));
    let staging = Arc::new(StagingStore::new(&temp.path().join("staging"))?);

    let entries = vec![
        WalEntry {
            seq: 1,
            timestamp_unix_nanos: 1,
            payload: Some(WalPayload::Mkdir(MkdirMutation {
                path: "src".to_string(),
            })),
        },
        WalEntry {
            seq: 2,
            timestamp_unix_nanos: 2,
            payload: Some(WalPayload::CreateFile(CreateFileMutation {
                path: "src/main.rs".to_string(),
                inode_type: InodeType::RegularFile as i32,
                mode: 0o100644,
            })),
        },
        WalEntry {
            seq: 3,
            timestamp_unix_nanos: 3,
            payload: Some(WalPayload::Write(WriteMutation {
                path: "src/main.rs".to_string(),
                offset: 0,
                data: b"fn main() { println!(\"wal recovery\"); }".to_vec(),
            })),
        },
        WalEntry {
            seq: 4,
            timestamp_unix_nanos: 4,
            payload: Some(WalPayload::CreateFile(CreateFileMutation {
                path: "scratch.txt".to_string(),
                inode_type: InodeType::RegularFile as i32,
                mode: 0o100644,
            })),
        },
        WalEntry {
            seq: 5,
            timestamp_unix_nanos: 5,
            payload: Some(WalPayload::Write(WriteMutation {
                path: "scratch.txt".to_string(),
                offset: 0,
                data: b"temporary".to_vec(),
            })),
        },
        WalEntry {
            seq: 6,
            timestamp_unix_nanos: 6,
            payload: Some(WalPayload::Remove(RemoveMutation {
                path: "scratch.txt".to_string(),
            })),
        },
        WalEntry {
            seq: 7,
            timestamp_unix_nanos: 7,
            payload: Some(WalPayload::CreateFile(CreateFileMutation {
                path: "old_name.txt".to_string(),
                inode_type: InodeType::RegularFile as i32,
                mode: 0o100644,
            })),
        },
        WalEntry {
            seq: 8,
            timestamp_unix_nanos: 8,
            payload: Some(WalPayload::Write(WriteMutation {
                path: "old_name.txt".to_string(),
                offset: 0,
                data: b"truncate and rename me please".to_vec(),
            })),
        },
        WalEntry {
            seq: 9,
            timestamp_unix_nanos: 9,
            payload: Some(WalPayload::Truncate(TruncateMutation {
                path: "old_name.txt".to_string(),
                new_size: 8,
            })),
        },
        WalEntry {
            seq: 10,
            timestamp_unix_nanos: 10,
            payload: Some(WalPayload::Rename(RenameMutation {
                from_path: "old_name.txt".to_string(),
                to_path: "renamed.txt".to_string(),
            })),
        },
    ];

    replay_wal_entries(&entries, &vfs, &staging, &git_engine).await?;

    // Verify scratch.txt was removed
    let scratch_node = vfs.lookup_path("scratch.txt");
    assert!(scratch_node.is_err());

    // Verify old_name.txt was renamed to renamed.txt
    assert!(vfs.lookup_path("old_name.txt").is_err());
    let renamed_id = vfs.lookup_path("renamed.txt")?;
    let renamed_content = staging.get_staged_content(renamed_id)?.unwrap();
    assert_eq!(&renamed_content[..], b"truncate");

    // Verify src/main.rs exists and has modifications
    let main_node_id = vfs.lookup_path("src/main.rs")?;
    assert!(staging.is_staged(main_node_id));
    let content = staging.get_staged_content(main_node_id)?.unwrap();
    assert_eq!(&content[..], b"fn main() { println!(\"wal recovery\"); }");

    // Build packfile from recovered state
    let pack_res = build_pack_from_recovered_state(
        &vfs,
        &staging,
        &git_engine,
        base_commit,
        "Author <author@example.com>",
        "Recovered commit",
    )
    .await?
    .expect("Expected recovered packfile");

    assert!(!pack_res.commit_oid.is_empty());
    assert!(!pack_res.pack_data.is_empty());
    assert!(!pack_res.idx_data.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_multi_wal_crash_recovery_cumulative() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let wal_dir = temp.path().join("wal");
    let backend: Arc<dyn WalBackend> = Arc::new(LocalWalBackend::new(&wal_dir)?);

    let git_engine = Arc::new(GitEngine::new("https://example.com/repo.git", Some(temp.path()))?);
    let base_tree = "0000000000000000000000000000000000000000";
    let base_commit = "1111111111111111111111111111111111111111";

    // Session 0 crashes without .done
    let mut writer0 = backend.open_writer(0).await?;
    let header0 = WalEntry {
        seq: 1,
        timestamp_unix_nanos: 1,
        payload: Some(WalPayload::Header(WalHeader {
            repo_url: "https://example.com/repo.git".to_string(),
            base_commit_oid: base_commit.to_string(),
            author: "Author 1 <a1@example.com>".to_string(),
            commit_message: "Commit 1".to_string(),
            started_at_unix_secs: 100,
            wal_seq: 0,
        })),
    };
    let write0 = WalEntry {
        seq: 2,
        timestamp_unix_nanos: 2,
        payload: Some(WalPayload::Write(WriteMutation {
            path: "file1.txt".to_string(),
            offset: 0,
            data: b"content from crashed session 0".to_vec(),
        })),
    };
    writer0.append_entry(&header0).await?;
    writer0.append_entry(&write0).await?;
    writer0.flush().await?;
    // Notice: writer0 crashes! (no finalize, no mark_log_done)

    // Session 1 also crashes without .done
    let mut writer1 = backend.open_writer(1).await?;
    let header1 = WalEntry {
        seq: 1,
        timestamp_unix_nanos: 3,
        payload: Some(WalPayload::Header(WalHeader {
            repo_url: "https://example.com/repo.git".to_string(),
            base_commit_oid: base_commit.to_string(),
            author: "Author 2 <a2@example.com>".to_string(),
            commit_message: "Commit 2".to_string(),
            started_at_unix_secs: 200,
            wal_seq: 1,
        })),
    };
    let write1 = WalEntry {
        seq: 2,
        timestamp_unix_nanos: 4,
        payload: Some(WalPayload::Write(WriteMutation {
            path: "file2.txt".to_string(),
            offset: 0,
            data: b"content from crashed session 1".to_vec(),
        })),
    };
    writer1.append_entry(&header1).await?;
    writer1.append_entry(&write1).await?;
    writer1.flush().await?;

    // Now Session 2 starts up and recovers from the 2 crashes!
    let recovered = WalManager::recover_and_replay_uncommitted(
        &backend,
        &git_engine,
        base_tree,
        base_commit,
        "Fallback <f@example.com>",
        "Fallback msg",
        temp.path(),
        None,
    )
    .await?
    .expect("Expected crash recovery to produce a packfile");

    assert!(!recovered.commit_oid.is_empty());
    assert!(!recovered.root_tree_oid.is_empty());

    // Apply recovered objects to git_engine
    git_engine.apply_recovered_objects(
        &recovered.commit_oid,
        &recovered.root_tree_oid,
        &recovered.objects,
    );

    // Initialize new VFS from recovered root tree OID
    let restored_vfs = VfsManager::new(git_engine.clone(), &recovered.root_tree_oid);
    let f1_id = restored_vfs.lookup_path("file1.txt")?;
    let f2_id = restored_vfs.lookup_path("file2.txt")?;
    let f1_node = restored_vfs.get_node(f1_id).unwrap();
    let f2_node = restored_vfs.get_node(f2_id).unwrap();
    let f1_blob = git_engine.get_blob(&f1_node.oid.read()).await?;
    let f2_blob = git_engine.get_blob(&f2_node.oid.read()).await?;
    assert_eq!(f1_blob, b"content from crashed session 0");
    assert_eq!(f2_blob, b"content from crashed session 1");

    // Verify both 0.done and 1.done markers were created
    assert!(backend.is_log_done(0).await?);
    assert!(backend.is_log_done(1).await?);

    // If recover is called again, there should be nothing left to recover
    let second_run = WalManager::recover_and_replay_uncommitted(
        &backend,
        &git_engine,
        &recovered.root_tree_oid,
        &recovered.commit_oid,
        "Fallback <f@example.com>",
        "Fallback msg",
        temp.path(),
        None,
    )
    .await?;
    assert!(second_run.is_none());

    Ok(())
}
