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
    assert!(copying_text.contains("GNU GENERAL PUBLIC LICENSE") || copying_text.contains("GPL"));

    Ok(())
}
