use tempfile::TempDir;

use git_nfs::git::GitEngine;

#[tokio::test]
async fn test_gcs_git_storage_live_resolution_and_blob_read() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp = TempDir::new()?;
    let engine = GitEngine::new_gcs("git-on-gcs-rapid", "linux/", Some(temp.path())).await?;

    // 1. Initialize engine (resolves HEAD and loads all trees)
    let root_oid = engine.initialize(None).await?;
    assert!(!root_oid.is_empty());
    println!("Successfully initialized GCS Git Engine with root tree: {root_oid}");

    let base_commit = engine.base_commit_oid().expect("Base commit must be present");
    assert_eq!(base_commit.len(), 40);
    println!("Base commit OID: {base_commit}");

    // 2. Verify root tree is loaded
    let root_tree = engine.get_tree(&root_oid).expect("Root tree must be in memory");
    println!("Root tree entry count: {}", root_tree.entries.len());
    assert!(!root_tree.entries.is_empty());

    // 3. Find COPYING or Makefile entry in root tree
    let copying_entry = root_tree
        .entries
        .iter()
        .find(|e| e.name == "COPYING")
        .expect("COPYING file must exist in Linux root tree");

    println!("Found COPYING entry with OID: {}", copying_entry.oid);

    // 4. Fetch blob content
    let blob_bytes = engine.get_blob(&copying_entry.oid).await?;
    assert!(!blob_bytes.is_empty());
    let copying_text = String::from_utf8_lossy(&blob_bytes);
    println!("COPYING header:\n{}", &copying_text[..std::cmp::min(200, copying_text.len())]);
    // 5. Verify block cache behavior:
    // Verify full packfile was NOT downloaded, and chunk blocks WERE created!
    let gcs_repo_pack = temp.path().join("gcs_repo").join("objects").join("pack");
    let mut found_blocks_dir = false;
    let mut found_full_pack = false;

    if let Ok(entries) = std::fs::read_dir(&gcs_repo_pack) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.extension().and_then(|s| s.to_str()) == Some("blocks") {
                found_blocks_dir = true;
                // Verify blocks are 10 MiB or less
                for block_entry in std::fs::read_dir(&path)?.flatten() {
                    let len = block_entry.metadata()?.len();
                    assert!(
                        len <= 10 * 1024 * 1024,
                        "Block file {} is larger than 10 MiB: {} bytes",
                        block_entry.path().display(),
                        len
                    );
                }
            } else if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("pack") {
                found_full_pack = true;
            }
        }
    }

    assert!(found_blocks_dir, "Block cache directory (.blocks) must exist");
    assert!(!found_full_pack, "Full packfile should NOT be downloaded to disk");

    Ok(())
}

#[tokio::test]
async fn test_gcs_git_storage_linux_full_chunked_reading() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let temp = TempDir::new()?;
    // Test against the exact repo that caused the user issue: linux-full/
    let engine = GitEngine::new_gcs("git-on-gcs-rapid", "linux-full/", Some(temp.path())).await?;

    let root_oid = engine.initialize(None).await?;
    assert!(!root_oid.is_empty());

    let root_tree = engine.get_tree(&root_oid).expect("Root tree must be loaded");
    assert!(!root_tree.entries.is_empty());

    // Verify full packfile was NOT downloaded
    let gcs_repo_pack = temp.path().join("gcs_repo").join("objects").join("pack");
    for entry in std::fs::read_dir(&gcs_repo_pack)?.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("pack") {
            panic!("Full packfile {} was downloaded! Should use 10 MiB block cache instead.", path.display());
        }
    }

    Ok(())
}
