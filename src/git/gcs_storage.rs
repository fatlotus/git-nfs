use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use flate2::read::ZlibDecoder;
use google_cloud_storage::client::{Storage, StorageControl};
use google_cloud_storage::model_ext::ReadRange;
use parking_lot::RwLock;
use tracing::info;

use crate::cache::block::{BlockCache, DEFAULT_BLOCK_SIZE};
use crate::git::idx::PackIndex;
use crate::git::pack::{apply_git_delta, GitRawObject, ObjectType};

pub struct GcsGitStorage {
    storage: Storage,
    control: StorageControl,
    bucket: String,
    prefix: String,
    cache_dir: PathBuf,
    block_cache: Arc<BlockCache>,
    pack_indexes: RwLock<Vec<PackIndex>>,
    // Cache of raw resolved objects by OID to deduplicate repeated delta resolution
    object_cache: RwLock<HashMap<String, Arc<GitRawObject>>>,
}

impl GcsGitStorage {
    pub async fn new(bucket: &str, prefix: &str, cache_dir: &Path) -> Result<Self> {
        let storage = Storage::builder().build().await.context("Building Storage client")?;
        let control = StorageControl::builder().build().await.context("Building StorageControl client")?;

        let clean_prefix = prefix.trim_start_matches('/').to_string();
        let formatted_bucket = if bucket.starts_with("projects/") {
            bucket.to_string()
        } else {
            format!("projects/_/buckets/{bucket}")
        };

        let storage_cache_dir = cache_dir.join("gcs_repo");
        fs::create_dir_all(&storage_cache_dir).context("Creating GCS repo cache dir")?;

        let block_cache = Arc::new(BlockCache::new(
            storage_cache_dir.clone(),
            DEFAULT_BLOCK_SIZE,
        ));

        let s = Self {
            storage,
            control,
            bucket: formatted_bucket,
            prefix: clean_prefix,
            cache_dir: storage_cache_dir,
            block_cache,
            pack_indexes: RwLock::new(Vec::new()),
            object_cache: RwLock::new(HashMap::new()),
        };

        // Initial discovery of pack indexes
        s.refresh_pack_indexes().await?;

        Ok(s)
    }

    pub fn pack_names(&self) -> Vec<String> {
        self.pack_indexes
            .read()
            .iter()
            .map(|idx| idx.pack_name.clone())
            .collect()
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn block_cache(&self) -> &Arc<BlockCache> {
        &self.block_cache
    }

    pub fn get_pack_object_len(&self, pack_name: &str, offset: u64) -> Option<usize> {
        let indexes = self.pack_indexes.read();
        for idx in indexes.iter() {
            if idx.pack_name == pack_name {
                return idx.get_object_len_at_offset(offset);
            }
        }
        None
    }

    /// Resolves the commit OID for HEAD or a specific branch/tag.
    pub async fn resolve_head(&self, branch_override: Option<&str>) -> Result<(String, String)> {
        // 1. If branch override is specified, attempt to read refs/heads/<branch>
        if let Some(branch) = branch_override {
            let ref_path = format!("{}refs/heads/{branch}", self.prefix);
            if let Ok(bytes) = self.read_full_object(&ref_path).await {
                let sha = String::from_utf8_lossy(&bytes).trim().to_string();
                if sha.len() == 40 {
                    info!("Resolved branch '{branch}' to commit {sha}");
                    return Ok((sha, branch.to_string()));
                }
            }

            // Check packed-refs
            if let Some(sha) = self.lookup_packed_ref(&format!("refs/heads/{branch}")).await? {
                info!("Resolved branch '{branch}' from packed-refs to commit {sha}");
                return Ok((sha, branch.to_string()));
            }
        }

        // 2. Read HEAD
        let head_path = format!("{}HEAD", self.prefix);
        let head_bytes = self
            .read_full_object(&head_path)
            .await
            .with_context(|| format!("Reading HEAD object from {}", head_path))?;
        let head_str = String::from_utf8_lossy(&head_bytes).trim().to_string();

        if head_str.starts_with("ref: ") {
            let ref_name = head_str.trim_start_matches("ref: ").trim();
            let ref_path = format!("{}{ref_name}", self.prefix);

            if let Ok(bytes) = self.read_full_object(&ref_path).await {
                let sha = String::from_utf8_lossy(&bytes).trim().to_string();
                if sha.len() == 40 {
                    info!("Resolved HEAD ({ref_name}) to commit {sha}");
                    return Ok((sha, ref_name.to_string()));
                }
            }

            // Check packed-refs
            if let Some(sha) = self.lookup_packed_ref(ref_name).await? {
                info!("Resolved HEAD ({ref_name}) from packed-refs to commit {sha}");
                return Ok((sha, ref_name.to_string()));
            }

            Err(anyhow!("Failed to resolve symbolic ref '{ref_name}'"))
        } else if head_str.len() >= 40 {
            // Detached HEAD containing direct commit SHA
            let sha = head_str[..40].to_string();
            info!("Resolved detached HEAD to commit {sha}");
            Ok((sha, "HEAD".to_string()))
        } else {
            Err(anyhow!("Invalid or empty HEAD in repository: '{head_str}'"))
        }
    }

    async fn lookup_packed_ref(&self, ref_name: &str) -> Result<Option<String>> {
        let packed_refs_path = format!("{}packed-refs", self.prefix);
        if let Ok(bytes) = self.read_full_object(&packed_refs_path).await {
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == ref_name {
                    return Ok(Some(parts[0].to_string()));
                }
            }
        }
        Ok(None)
    }

    /// Discovers all .idx files in objects/pack/ and loads them.
    pub async fn refresh_pack_indexes(&self) -> Result<()> {
        let pack_prefix = format!("{}objects/pack/", self.prefix);
        let list_builder = self
            .control
            .list_objects()
            .set_parent(&self.bucket)
            .set_prefix(&pack_prefix);

        let resp = list_builder
            .send()
            .await
            .with_context(|| format!("Listing objects in {} with prefix {}", self.bucket, pack_prefix))?;

        let mut loaded_names = Vec::new();
        {
            let indexes = self.pack_indexes.read();
            for idx in indexes.iter() {
                loaded_names.push(idx.pack_name.clone());
            }
        }

        let mut new_indexes = Vec::new();

        for obj in resp.objects {
            let name = obj.name;
            if name.ends_with(".idx") {
                let pack_obj_name = format!("{}.pack", name.strip_suffix(".idx").unwrap());
                if loaded_names.contains(&pack_obj_name) {
                    continue;
                }

                info!("Loading Git pack index: {name}");
                let idx_bytes = self.read_full_object(&name).await?;
                let pack_idx = PackIndex::parse(&pack_obj_name, &idx_bytes)
                    .with_context(|| format!("Parsing pack index {name}"))?;

                info!(
                    "Loaded pack index {name}: {} objects for pack {}",
                    pack_idx.object_count(),
                    pack_obj_name
                );
                new_indexes.push(pack_idx);
            }
        }

        if !new_indexes.is_empty() {
            let mut indexes = self.pack_indexes.write();
            indexes.extend(new_indexes);
        }

        Ok(())
    }

    /// Ensures a packfile is downloaded to the local disk cache for sub-millisecond local reads.
    pub async fn ensure_pack_cached(&self, pack_obj_name: &str) -> Result<PathBuf> {
        let filename = pack_obj_name
            .rsplit('/')
            .next()
            .unwrap_or(pack_obj_name);
        let local_pack_path = self.cache_dir.join("objects").join("pack").join(filename);

        if local_pack_path.is_file() {
            return Ok(local_pack_path);
        }

        if let Some(p) = local_pack_path.parent() {
            fs::create_dir_all(p)?;
        }

        info!("Caching packfile from GCS to local disk: {pack_obj_name} -> {}", local_pack_path.display());
        let pack_bytes = self.read_full_object(pack_obj_name).await?;
        fs::write(&local_pack_path, pack_bytes)
            .with_context(|| format!("Writing cached packfile {}", local_pack_path.display()))?;

        info!("Successfully cached {} to local disk", filename);
        Ok(local_pack_path)
    }

    /// Reads a full object's byte content from GCS.
    pub async fn read_full_object(&self, object_name: &str) -> Result<Vec<u8>> {
        let mut reader = self
            .storage
            .read_object(&self.bucket, object_name)
            .send()
            .await
            .with_context(|| format!("Reading object {} from {}", object_name, self.bucket))?;

        let mut data = Vec::new();
        while let Some(chunk_res) = reader.next().await {
            let chunk = chunk_res.context("Streaming object chunk from GCS")?;
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    /// Reads a raw Git object (commit, tree, blob) by OID.
    pub async fn read_object_raw(&self, oid: &str) -> Result<GitRawObject> {
        // 1. Check in-memory object cache
        if let Some(obj) = self.object_cache.read().get(oid) {
            return Ok((**obj).clone());
        }

        let sha_bytes: [u8; 20] = hex::decode(oid)
            .with_context(|| format!("Invalid hex SHA: {oid}"))?
            .try_into()
            .map_err(|_| anyhow!("Invalid SHA length"))?;

        // 2. Search across packfile indexes
        let pack_match = {
            let indexes = self.pack_indexes.read();
            let mut m = None;
            for idx in indexes.iter() {
                if let Some((offset, len)) = idx.find_offset(&sha_bytes) {
                    m = Some((idx.pack_name.clone(), offset, len));
                    break;
                }
            }
            m
        };

        let raw_obj = if let Some((pack_name, offset, len)) = pack_match {
            self.read_pack_object(&pack_name, offset, len).await?
        } else {
            // 3. Fallback: Loose object under objects/??/????????...
            let loose_path = format!("{}objects/{}/{}", self.prefix, &oid[..2], &oid[2..]);
            let compressed = self
                .read_full_object(&loose_path)
                .await
                .with_context(|| format!("Object {oid} not found in pack indexes or loose objects"))?;

            let mut decoder = ZlibDecoder::new(&compressed[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)?;

            // Parse header: "<type> <len>\0<data>"
            let null_pos = decompressed
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| anyhow!("Corrupt loose object {oid}: missing null byte in header"))?;
            let header = String::from_utf8_lossy(&decompressed[..null_pos]);
            let obj_type = if header.starts_with("commit ") {
                ObjectType::Commit
            } else if header.starts_with("tree ") {
                ObjectType::Tree
            } else if header.starts_with("blob ") {
                ObjectType::Blob
            } else if header.starts_with("tag ") {
                ObjectType::Tag
            } else {
                return Err(anyhow!("Unknown loose object type in header: {header}"));
            };

            GitRawObject {
                obj_type,
                data: decompressed[null_pos + 1..].to_vec(),
            }
        };

        // Cache resolved object
        self.object_cache
            .write()
            .insert(oid.to_string(), Arc::new(raw_obj.clone()));

        Ok(raw_obj)
    }

    /// Reads a slice of a packfile via the BlockCache (10 MiB chunked reads).
    pub async fn read_pack_slice(
        &self,
        pack_name: &str,
        offset: u64,
        read_len: usize,
    ) -> Result<Vec<u8>> {
        let storage = self.storage.clone();
        let bucket = self.bucket.clone();
        let p_name = pack_name.to_string();

        let fetcher = move |block_offset: u64, block_len: usize| {
            let s = storage.clone();
            let b = bucket.clone();
            let name = p_name.clone();
            async move {
                let mut reader = s
                    .read_object(&b, &name)
                    .set_read_range(ReadRange::segment(block_offset, block_len as u64))
                    .send()
                    .await
                    .with_context(|| {
                        format!("Reading pack range {block_offset}..+{} from {name}", block_len)
                    })?;

                let mut buf = Vec::with_capacity(block_len);
                while let Some(chunk_res) = reader.next().await {
                    let chunk = chunk_res?;
                    buf.extend_from_slice(&chunk);
                }
                Ok(buf)
            }
        };

        self.block_cache
            .read_range(pack_name, offset, read_len, fetcher)
            .await
    }

    /// Identifies all unique packfile blocks needed for the specified object OIDs,
    /// and prefetches any uncached blocks concurrently from GCS.
    pub async fn prefetch_blocks_for_objects(&self, oids: &[String]) -> Result<()> {
        let block_size = self.block_cache.block_size() as u64;
        let mut needed_blocks = HashSet::new();

        {
            let indexes = self.pack_indexes.read();
            let cache = self.object_cache.read();
            for oid in oids {
                // If already in memory object cache, no block read is needed
                if cache.contains_key(oid) {
                    continue;
                }

                if let Ok(bytes) = hex::decode(oid) {
                    if let Ok(sha) = <[u8; 20]>::try_from(bytes.as_slice()) {
                        for idx in indexes.iter() {
                            if let Some((offset, _)) = idx.find_offset(&sha) {
                                let block_idx = offset / block_size;
                                if !self.block_cache.has_block(&idx.pack_name, block_idx) {
                                    needed_blocks.insert((idx.pack_name.clone(), block_idx));
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        if needed_blocks.is_empty() {
            return Ok(());
        }

        info!(
            "Prefetching {} uncached pack blocks in parallel for {} objects...",
            needed_blocks.len(),
            oids.len()
        );

        let semaphore = Arc::new(tokio::sync::Semaphore::new(16));
        let mut tasks = Vec::with_capacity(needed_blocks.len());

        for (pack_name, block_idx) in needed_blocks {
            let s = self.storage.clone();
            let b = self.bucket.clone();
            let p_name = pack_name.clone();
            let bc = self.block_cache.clone();
            let sem = semaphore.clone();

            tasks.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.ok();
                let fetcher = move |block_offset: u64, block_len: usize| {
                    let s = s.clone();
                    let b = b.clone();
                    let name = p_name.clone();
                    async move {
                        let mut reader = s
                            .read_object(&b, &name)
                            .set_read_range(ReadRange::segment(block_offset, block_len as u64))
                            .send()
                            .await
                            .with_context(|| {
                                format!("Reading pack range {block_offset}..+{} from {name}", block_len)
                            })?;

                        let mut buf = Vec::with_capacity(block_len);
                        while let Some(chunk_res) = reader.next().await {
                            let chunk = chunk_res?;
                            buf.extend_from_slice(&chunk);
                        }
                        Ok(buf)
                    }
                };

                bc.get_or_fetch_block(&pack_name, block_idx, &fetcher).await
            }));
        }

        for task in futures::future::join_all(tasks).await {
            if let Ok(Err(e)) = task {
                tracing::warn!("Error prefetching pack block: {e}");
            }
        }

        Ok(())
    }

    /// Reads an object located at offset in a packfile (resolving deltas if needed).
    #[async_recursion::async_recursion]
    pub async fn read_pack_object(
        &self,
        pack_name: &str,
        offset: u64,
        estimated_len: usize,
    ) -> Result<GitRawObject> {
        let read_len = std::cmp::max(estimated_len + 64, 4096);
        let pack_slice = self.read_pack_slice(pack_name, offset, read_len).await?;

        if pack_slice.is_empty() {
            return Err(anyhow!("Empty read from pack {pack_name} at offset {offset}"));
        }

        // Decode object header
        let mut pos = 0;
        let mut c = pack_slice[pos];
        pos += 1;

        let type_num = (c >> 4) & 7;
        let obj_type = ObjectType::from_u8(type_num)?;

        let mut size = (c & 15) as usize;
        let mut shift = 4;
        while (c & 0x80) != 0 {
            if pos >= pack_slice.len() {
                return Err(anyhow!("Truncated varint in pack object header"));
            }
            c = pack_slice[pos];
            pos += 1;
            size |= ((c & 0x7f) as usize) << shift;
            shift += 7;
        }

        match obj_type {
            ObjectType::OfsDelta => {
                if pos >= pack_slice.len() {
                    return Err(anyhow!("Truncated OFS_DELTA offset"));
                }
                c = pack_slice[pos];
                pos += 1;
                let mut base_offset_delta = (c & 0x7f) as usize;
                while (c & 0x80) != 0 {
                    if pos >= pack_slice.len() {
                        return Err(anyhow!("Truncated OFS_DELTA continuation"));
                    }
                    c = pack_slice[pos];
                    pos += 1;
                    base_offset_delta = ((base_offset_delta + 1) << 7) | ((c & 0x7f) as usize);
                }
                let base_offset = offset
                    .checked_sub(base_offset_delta as u64)
                    .ok_or_else(|| anyhow!("Invalid negative base offset in OFS_DELTA"))?;

                let mut decoder = ZlibDecoder::new(&pack_slice[pos..]);
                let mut delta_data = Vec::new();
                decoder.read_to_end(&mut delta_data)?;

                // Recursively resolve base object
                let base_len = self.get_pack_object_len(pack_name, base_offset).unwrap_or(size);
                let base_obj = self.read_pack_object(pack_name, base_offset, base_len).await?;
                let resolved = apply_git_delta(&base_obj.data, &delta_data)?;

                Ok(GitRawObject {
                    obj_type: base_obj.obj_type,
                    data: resolved,
                })
            }
            ObjectType::RefDelta => {
                if pos + 20 > pack_slice.len() {
                    return Err(anyhow!("Truncated REF_DELTA SHA"));
                }
                let base_sha = hex::encode(&pack_slice[pos..pos + 20]);
                pos += 20;

                let mut decoder = ZlibDecoder::new(&pack_slice[pos..]);
                let mut delta_data = Vec::new();
                decoder.read_to_end(&mut delta_data)?;

                // Recursively resolve base object by SHA
                let base_obj = self.read_object_raw(&base_sha).await?;
                let resolved = apply_git_delta(&base_obj.data, &delta_data)?;

                Ok(GitRawObject {
                    obj_type: base_obj.obj_type,
                    data: resolved,
                })
            }
            base_type => {
                let mut decoder = ZlibDecoder::new(&pack_slice[pos..]);
                let mut decomp = Vec::with_capacity(size);
                decoder.read_to_end(&mut decomp)?;

                Ok(GitRawObject {
                    obj_type: base_type,
                    data: decomp,
                })
            }
        }
    }

    pub async fn write_rapid_object(&self, object_name: &str, data: &[u8]) -> Result<()> {
        let mut writer = self
            .storage
            .open_appendable_object(&self.bucket, object_name)
            .send()
            .await
            .with_context(|| format!("Opening appendable object {} in {}", object_name, self.bucket))?;

        if !data.is_empty() {
            writer
                .append(Bytes::copy_from_slice(data))
                .await
                .with_context(|| format!("Appending to {}", object_name))?;
            writer
                .flush()
                .await
                .with_context(|| format!("Flushing {}", object_name))?;
        }
        writer
            .finalize()
            .await
            .with_context(|| format!("Finalizing {}", object_name))?;
        Ok(())
    }

    /// Writes a completed packfile, index, and commit ref to the GCS Rapid repository.
    /// Order:
    /// 1. Upload packfile (pack-<sha>.pack)
    /// 2. Upload index file (pack-<sha>.idx)
    /// 3. Update Git refs (HEAD and active branch)
    /// Performs the equivalent of "git update-server-info" for static HTTP hosting (Dumb HTTP).
    /// Generates and uploads info/refs and objects/info/packs to GCS and writes them to local cache.
    pub async fn update_server_info(
        &self,
        target_branch: &str,
        new_commit_oid: &str,
    ) -> Result<()> {
        info!("Updating server info (info/refs and objects/info/packs) on GCS...");

        // 1. Collect references:
        // Key: ref_name -> (sha, peeled_sha_opt)
        let mut refs_map: HashMap<String, (String, Option<String>)> = HashMap::new();

        // A. Read packed-refs if present
        let packed_refs_path = format!("{}packed-refs", self.prefix);
        if let Ok(bytes) = self.read_full_object(&packed_refs_path).await {
            let text = String::from_utf8_lossy(&bytes);
            let mut last_ref_name: Option<String> = None;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                if line.starts_with('^') {
                    let peeled_sha = line.trim_start_matches('^').trim();
                    if peeled_sha.len() == 40 {
                        if let Some(ref r_name) = last_ref_name {
                            if let Some(entry) = refs_map.get_mut(r_name) {
                                entry.1 = Some(peeled_sha.to_string());
                            }
                        }
                    }
                } else {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 && parts[0].len() == 40 {
                        let sha = parts[0].to_string();
                        let ref_name = parts[1].to_string();
                        refs_map.insert(ref_name.clone(), (sha, None));
                        last_ref_name = Some(ref_name);
                    }
                }
            }
        }

        // B. Discover loose refs under refs/ in GCS
        let refs_prefix = format!("{}refs/", self.prefix);
        let list_builder = self
            .control
            .list_objects()
            .set_parent(&self.bucket)
            .set_prefix(&refs_prefix);

        if let Ok(resp) = list_builder.send().await {
            for obj in resp.objects {
                let obj_name = obj.name;
                if let Some(ref_name) = obj_name.strip_prefix(&self.prefix) {
                    if let Ok(bytes) = self.read_full_object(&obj_name).await {
                        let sha = String::from_utf8_lossy(&bytes).trim().to_string();
                        if sha.len() == 40 {
                            // Loose ref overrides packed-refs
                            let peeled = refs_map.get(ref_name).and_then(|e| e.1.clone());
                            refs_map.insert(ref_name.to_string(), (sha, peeled));
                        }
                    }
                }
            }
        }

        // C. Explicitly ensure target branch ref is updated to new_commit_oid
        let target_ref_name = format!("refs/heads/{target_branch}");
        refs_map.insert(target_ref_name, (new_commit_oid.to_string(), None));

        // Format info/refs content
        let ref_entries: Vec<(String, String, Option<String>)> = refs_map
            .into_iter()
            .map(|(name, (sha, peeled))| (name, sha, peeled))
            .collect();
        let info_refs_content = format_info_refs(&ref_entries);

        // Upload info/refs to GCS
        let info_refs_path = format!("{}info/refs", self.prefix);
        self.write_rapid_object(&info_refs_path, info_refs_content.as_bytes()).await?;

        // Cache info/refs locally
        let local_info_refs = self.cache_dir.join("info").join("refs");
        if let Some(p) = local_info_refs.parent() {
            let _ = fs::create_dir_all(p);
        }
        let _ = fs::write(&local_info_refs, info_refs_content.as_bytes());

        // 2. Discover packfiles for objects/info/packs
        let mut pack_names: Vec<String> = self
            .pack_names()
            .into_iter()
            .map(|p| p.rsplit('/').next().unwrap_or(&p).to_string())
            .collect();

        let pack_prefix = format!("{}objects/pack/", self.prefix);
        let list_packs = self
            .control
            .list_objects()
            .set_parent(&self.bucket)
            .set_prefix(&pack_prefix);

        if let Ok(resp) = list_packs.send().await {
            for obj in resp.objects {
                if obj.name.ends_with(".pack") {
                    let basename = obj.name.rsplit('/').next().unwrap_or(&obj.name).to_string();
                    pack_names.push(basename);
                }
            }
        }

        let info_packs_content = format_info_packs(&pack_names);

        // Upload objects/info/packs to GCS
        let info_packs_path = format!("{}objects/info/packs", self.prefix);
        self.write_rapid_object(&info_packs_path, info_packs_content.as_bytes()).await?;

        // Cache objects/info/packs locally
        let local_info_packs = self.cache_dir.join("objects").join("info").join("packs");
        if let Some(p) = local_info_packs.parent() {
            let _ = fs::create_dir_all(p);
        }
        let _ = fs::write(&local_info_packs, info_packs_content.as_bytes());

        info!("Successfully updated info/refs and objects/info/packs on GCS");
        Ok(())
    }

    /// Writes a completed packfile, index, and commit ref to the GCS Rapid repository.
    /// Order:
    /// 1. Upload packfile (pack-<sha>.pack)
    /// 2. Upload index file (pack-<sha>.idx)
    /// 3. Update Git refs (HEAD and active branch)
    /// 4. Update Dumb HTTP server info (info/refs and objects/info/packs)
    pub async fn write_completed_commit(
        &self,
        commit_oid: &str,
        pack_sha: &str,
        pack_data: &[u8],
        idx_data: &[u8],
        branch: Option<&str>,
    ) -> Result<()> {
        let pack_obj_name = format!("{}objects/pack/pack-{pack_sha}.pack", self.prefix);
        let idx_obj_name = format!("{}objects/pack/pack-{pack_sha}.idx", self.prefix);

        info!("Uploading standard packfile to GCS Rapid: {pack_obj_name} ({} bytes)", pack_data.len());
        self.write_rapid_object(&pack_obj_name, pack_data).await?;

        info!("Uploading index file to GCS Rapid: {idx_obj_name} ({} bytes)", idx_data.len());
        self.write_rapid_object(&idx_obj_name, idx_data).await?;

        // Cache locally as well
        let local_pack = self.cache_dir.join("objects").join("pack").join(format!("pack-{pack_sha}.pack"));
        let local_idx = self.cache_dir.join("objects").join("pack").join(format!("pack-{pack_sha}.idx"));
        if let Some(p) = local_pack.parent() {
            let _ = fs::create_dir_all(p);
        }
        let _ = fs::write(&local_pack, pack_data);
        let _ = fs::write(&local_idx, idx_data);

        // Update Git refs on GCS Rapid
        info!("Updating Git refs on GCS Rapid with commit {commit_oid}...");
        let commit_payload = format!("{commit_oid}\n").into_bytes();
        let target_branch = branch.unwrap_or("master");

        // 1. Update HEAD as a symbolic ref so git clone automatically checks out the branch
        let head_name = format!("{}HEAD", self.prefix);
        let head_payload = format!("ref: refs/heads/{target_branch}\n").into_bytes();
        self.write_rapid_object(&head_name, &head_payload).await?;

        let local_head = self.cache_dir.join("HEAD");
        let _ = fs::write(&local_head, &head_payload);

        // 2. Update active branch ref
        let branch_ref = format!("{}refs/heads/{target_branch}", self.prefix);
        self.write_rapid_object(&branch_ref, &commit_payload).await?;

        let local_branch_ref = self.cache_dir.join("refs").join("heads").join(target_branch);
        if let Some(p) = local_branch_ref.parent() {
            let _ = fs::create_dir_all(p);
        }
        let _ = fs::write(&local_branch_ref, &commit_payload);

        info!("Successfully updated HEAD and {branch_ref} to commit {commit_oid} on GCS Rapid");

        // Refresh pack indexes so the new pack is immediately available in memory
        self.refresh_pack_indexes().await?;

        // 3. Update server info files (info/refs and objects/info/packs) for Dumb HTTP git clone
        self.update_server_info(target_branch, commit_oid).await?;

        Ok(())
    }
}

/// Formats the content of info/refs from a slice of (ref_name, sha, peeled_sha_opt).
/// Returns entries sorted alphabetically by ref_name.
pub fn format_info_refs(entries: &[(String, String, Option<String>)]) -> String {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (ref_name, sha, peeled) in sorted {
        out.push_str(&format!("{sha}\t{ref_name}\n"));
        if let Some(peeled_sha) = peeled {
            out.push_str(&format!("{peeled_sha}\t{ref_name}^{{}}\n"));
        }
    }
    out
}

/// Formats the content of objects/info/packs from a slice of pack names.
/// Extracts basenames, deduplicates, sorts, and adds a trailing newline.
pub fn format_info_packs(pack_names: &[String]) -> String {
    let mut sorted: Vec<String> = pack_names
        .iter()
        .map(|name| name.rsplit('/').next().unwrap_or(name).to_string())
        .filter(|name| name.ends_with(".pack"))
        .collect();
    sorted.sort();
    sorted.dedup();
    let mut out = String::new();
    for name in sorted {
        out.push_str(&format!("P {name}\n"));
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_info_refs_sorting_and_peeling() {
        let entries = vec![
            (
                "refs/heads/master".to_string(),
                "1111111111111111111111111111111111111111".to_string(),
                None,
            ),
            (
                "refs/tags/v1.0".to_string(),
                "2222222222222222222222222222222222222222".to_string(),
                Some("3333333333333333333333333333333333333333".to_string()),
            ),
            (
                "refs/heads/feature".to_string(),
                "4444444444444444444444444444444444444444".to_string(),
                None,
            ),
        ];

        let formatted = format_info_refs(&entries);
        let expected = "\
4444444444444444444444444444444444444444\trefs/heads/feature
1111111111111111111111111111111111111111\trefs/heads/master
2222222222222222222222222222222222222222\trefs/tags/v1.0
3333333333333333333333333333333333333333\trefs/tags/v1.0^{}
";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_info_packs_dedup_and_sort() {
        let packs = vec![
            "objects/pack/pack-bbbb.pack".to_string(),
            "pack-aaaa.pack".to_string(),
            "pack-bbbb.pack".to_string(),
            "some/other/path/pack-cccc.pack".to_string(),
            "ignored-file.idx".to_string(),
        ];

        let formatted = format_info_packs(&packs);
        let expected = "\
P pack-aaaa.pack
P pack-bbbb.pack
P pack-cccc.pack

";
        assert_eq!(formatted, expected);
    }
}

