use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::write::ZlibEncoder;
use flate2::{Compression, Crc};
use sha1::{Digest, Sha1};
use tracing::info;

use crate::git::builder::GitObjectToPack;

struct ObjectEntry {
    oid_bytes: Vec<u8>,
    pack_offset: u64,
    crc32: u32,
}

pub struct PackWriter;

impl PackWriter {
    /// Generates in-memory pack and index file bytes for a list of Git objects,
    /// returning (pack_bytes, idx_bytes, pack_sha_hex).
    pub fn create_pack_and_index_bytes(
        objects: &[GitObjectToPack],
    ) -> Result<(Vec<u8>, Vec<u8>, String)> {
        let (pack_data, entries, pack_sha) = Self::create_pack_data(objects)?;
        let idx_data = Self::create_idx_v2_data(entries, &pack_sha)?;
        let pack_sha_hex = hex::encode(pack_sha);
        Ok((pack_data, idx_data, pack_sha_hex))
    }

    /// Writes a list of Git objects into a .pack file and its companion .idx file.
    pub fn write_pack_and_index(
        pack_path: &Path,
        idx_path: &Path,
        objects: &[GitObjectToPack],
    ) -> Result<()> {
        let (pack_data, idx_data, _pack_sha) = Self::create_pack_and_index_bytes(objects)?;

        // Write .pack file
        let mut pack_file = File::create(pack_path)
            .with_context(|| format!("Creating packfile at {}", pack_path.display()))?;
        pack_file.write_all(&pack_data)?;
        pack_file.flush()?;

        // Write .idx file
        let mut idx_file = File::create(idx_path)
            .with_context(|| format!("Creating index file at {}", idx_path.display()))?;
        idx_file.write_all(&idx_data)?;
        idx_file.flush()?;

        info!(
            "Generated Git packfile: {} ({} bytes) and index: {}",
            pack_path.display(),
            pack_data.len(),
            idx_path.display()
        );

        Ok(())
    }

    /// Generates raw packfile bytes and metadata needed for .idx.
    fn create_pack_data(
        objects: &[GitObjectToPack],
    ) -> Result<(Vec<u8>, Vec<ObjectEntry>, [u8; 20])> {
        let mut pack = Vec::new();
        let mut entries = Vec::with_capacity(objects.len());

        // 1. Pack Header (12 bytes)
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes()); // Version 2
        pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());

        // 2. Objects
        for obj in objects {
            let pack_offset = pack.len() as u64;

            // Encode object header (type + varint size)
            let type_num = obj.obj_type as u8;
            let mut size = obj.data.len();

            let mut byte = (type_num << 4) | ((size & 0x0f) as u8);
            size >>= 4;
            if size > 0 {
                byte |= 0x80;
            }
            pack.push(byte);

            while size > 0 {
                let mut b = (size & 0x7f) as u8;
                size >>= 7;
                if size > 0 {
                    b |= 0x80;
                }
                pack.push(b);
            }

            // Compress object data with zlib
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&obj.data)?;
            let compressed = encoder.finish()?;

            // Compute CRC32 over the object header + compressed payload in the pack
            let mut crc_hasher = Crc::new();
            crc_hasher.update(&pack[pack_offset as usize..]);
            crc_hasher.update(&compressed);
            let crc32 = crc_hasher.sum();

            pack.extend_from_slice(&compressed);

            let oid_bytes = hex::decode(&obj.oid)
                .with_context(|| format!("Invalid hex SHA-1: {}", obj.oid))?;

            entries.push(ObjectEntry {
                oid_bytes,
                pack_offset,
                crc32,
            });
        }

        // 3. Packfile Checksum (20 bytes SHA-1 over all preceding bytes)
        let mut pack_hasher = Sha1::new();
        pack_hasher.update(&pack);
        let pack_sha: [u8; 20] = pack_hasher.finalize().into();
        pack.extend_from_slice(&pack_sha);

        Ok((pack, entries, pack_sha))
    }

    /// Generates Git v2 index file bytes.
    fn create_idx_v2_data(
        mut entries: Vec<ObjectEntry>,
        pack_sha: &[u8; 20],
    ) -> Result<Vec<u8>> {
        // Sort entries strictly in ascending lexicographical order of binary SHA-1
        entries.sort_by(|a, b| a.oid_bytes.cmp(&b.oid_bytes));

        let mut idx = Vec::new();

        // 1. Index Header
        idx.extend_from_slice(b"\xfftOc"); // \xff\x74\x4f\x63
        idx.extend_from_slice(&2u32.to_be_bytes()); // Version 2

        // 2. First-level Fanout Table (256 * 4 = 1024 bytes)
        let mut fanout = [0u32; 256];
        for entry in &entries {
            let first_byte = entry.oid_bytes[0] as usize;
            fanout[first_byte] += 1;
        }
        // Cumulative count
        let mut cum = 0u32;
        for count in &mut fanout {
            cum += *count;
            *count = cum;
        }
        for count in &fanout {
            idx.extend_from_slice(&count.to_be_bytes());
        }

        // 3. Table of SHA-1s (N * 20 bytes)
        for entry in &entries {
            idx.extend_from_slice(&entry.oid_bytes);
        }

        // 4. Table of CRC32s (N * 4 bytes)
        for entry in &entries {
            idx.extend_from_slice(&entry.crc32.to_be_bytes());
        }

        // 5. Table of Offsets (N * 4 bytes)
        for entry in &entries {
            if entry.pack_offset > 0x7fff_ffff {
                // If larger than 2GB, bit 31 is set and points to 8-byte table
                // For changes.pack this is typically small, but we handle standard offset
                return Err(anyhow::anyhow!("Large pack offsets >2GB not implemented"));
            }
            idx.extend_from_slice(&(entry.pack_offset as u32).to_be_bytes());
        }

        // 6. Packfile Checksum (20 bytes)
        idx.extend_from_slice(pack_sha);

        // 7. Index Checksum (20 bytes SHA-1 over all preceding index bytes)
        let mut idx_hasher = Sha1::new();
        idx_hasher.update(&idx);
        let idx_sha = idx_hasher.finalize();
        idx.extend_from_slice(&idx_sha);

        Ok(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::pack::ObjectType;

    #[test]
    fn test_create_pack_and_index() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pack_path = temp_dir.path().join("test.pack");
        let idx_path = temp_dir.path().join("test.idx");

        let obj1 = GitObjectToPack {
            oid: "ce013625030ba8dba906f756967f9e9ca394464a".to_string(),
            obj_type: ObjectType::Blob,
            data: b"hello\n".to_vec(),
        };

        PackWriter::write_pack_and_index(&pack_path, &idx_path, &[obj1]).unwrap();

        assert!(pack_path.exists());
        assert!(idx_path.exists());

        let pack_bytes = std::fs::read(&pack_path).unwrap();
        assert_eq!(&pack_bytes[..4], b"PACK");
        assert_eq!(&pack_bytes[4..8], &2u32.to_be_bytes());
    }
}
