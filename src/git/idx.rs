use anyhow::{anyhow, Result};

#[derive(Debug, Clone)]
pub struct PackIndex {
    pub pack_name: String,
    fanout: [u32; 256],
    shas: Vec<[u8; 20]>,
    offsets: Vec<u64>,
    sorted_offsets: Vec<u64>,
    pack_sha: [u8; 20],
}

impl PackIndex {
    /// Parses a Git v2 .idx file buffer.
    pub fn parse(pack_name: &str, data: &[u8]) -> Result<Self> {
        if data.len() < 8 + 1024 + 40 {
            return Err(anyhow!("Index file too short: {} bytes", data.len()));
        }

        // 1. Verify Magic and Version
        if &data[0..4] != b"\xfftOc" {
            return Err(anyhow!("Invalid index magic: {:?}", &data[0..4]));
        }
        let version = u32::from_be_bytes(data[4..8].try_into()?);
        if version != 2 {
            return Err(anyhow!("Unsupported index version: {version} (expected 2)"));
        }

        // 2. Parse Fanout Table
        let mut fanout = [0u32; 256];
        for i in 0..256 {
            let start = 8 + i * 4;
            fanout[i] = u32::from_be_bytes(data[start..start + 4].try_into()?);
        }

        let num_objects = fanout[255] as usize;

        let shas_start = 8 + 1024;
        let shas_end = shas_start + num_objects * 20;
        let crcs_end = shas_end + num_objects * 4;
        let offsets_start = crcs_end;
        let offsets_end = offsets_start + num_objects * 4;

        if data.len() < offsets_end + 40 {
            return Err(anyhow!(
                "Index file truncated: expected at least {} bytes for {} objects, got {}",
                offsets_end + 40,
                num_objects,
                data.len()
            ));
        }

        // 3. Parse SHAs
        let mut shas = Vec::with_capacity(num_objects);
        for i in 0..num_objects {
            let start = shas_start + i * 20;
            let mut sha = [0u8; 20];
            sha.copy_from_slice(&data[start..start + 20]);
            shas.push(sha);
        }

        // 4. Parse Offsets
        let mut offsets = Vec::with_capacity(num_objects);
        let mut large_offset_indices = Vec::new();

        for i in 0..num_objects {
            let start = offsets_start + i * 4;
            let raw_offset = u32::from_be_bytes(data[start..start + 4].try_into()?);
            if (raw_offset & 0x8000_0000) != 0 {
                let large_idx = (raw_offset & 0x7fff_ffff) as usize;
                large_offset_indices.push((i, large_idx));
                offsets.push(0); // placeholder, filled below
            } else {
                offsets.push(raw_offset as u64);
            }
        }

        // Parse large offsets table if present
        if !large_offset_indices.is_empty() {
            let large_table_start = offsets_end;
            for (obj_idx, large_idx) in large_offset_indices {
                let start = large_table_start + large_idx * 8;
                if start + 8 > data.len() - 40 {
                    return Err(anyhow!("Large offset index out of bounds"));
                }
                let off = u64::from_be_bytes(data[start..start + 8].try_into()?);
                offsets[obj_idx] = off;
            }
        }

        // Parse pack SHA (preceding the last 20 bytes of index checksum)
        let pack_sha_offset = data.len() - 40;
        let mut pack_sha = [0u8; 20];
        pack_sha.copy_from_slice(&data[pack_sha_offset..pack_sha_offset + 20]);

        let mut sorted_offsets = offsets.clone();
        sorted_offsets.sort_unstable();

        Ok(Self {
            pack_name: pack_name.to_string(),
            fanout,
            shas,
            offsets,
            sorted_offsets,
            pack_sha,
        })
    }

    /// Finds the offset and estimated/exact compressed length of an object by SHA-1.
    pub fn find_offset(&self, sha: &[u8; 20]) -> Option<(u64, usize)> {
        let first_byte = sha[0] as usize;
        let low = if first_byte == 0 {
            0
        } else {
            self.fanout[first_byte - 1] as usize
        };
        let high = self.fanout[first_byte] as usize;

        if low >= high || high > self.shas.len() {
            return None;
        }

        let slice = &self.shas[low..high];
        match slice.binary_search(sha) {
            Ok(pos) => {
                let idx = low + pos;
                let offset = self.offsets[idx];

                // Determine compressed length by finding next offset
                let len = match self.sorted_offsets.binary_search(&offset) {
                    Ok(sorted_idx) => {
                        if sorted_idx + 1 < self.sorted_offsets.len() {
                            let next_off = self.sorted_offsets[sorted_idx + 1];
                            (next_off - offset) as usize
                        } else {
                            // Last object in pack - allocate a reasonable buffer (e.g. 64KB)
                            64 * 1024
                        }
                    }
                    Err(_) => 64 * 1024,
                };

                Some((offset, len))
            }
            Err(_) => None,
        }
    }

    pub fn object_count(&self) -> usize {
        self.shas.len()
    }

    pub fn pack_sha_hex(&self) -> String {
        hex::encode(self.pack_sha)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_idx() {
        // Construct minimal valid .idx v2
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\xfftOc");
        buf.extend_from_slice(&2u32.to_be_bytes());

        let mut fanout = [0u32; 256];
        fanout[0x12] = 1;
        for i in 0x13..256 {
            fanout[i] = 1;
        }
        for count in fanout {
            buf.extend_from_slice(&count.to_be_bytes());
        }

        // 1 SHA starting with 0x12
        let mut sha = [0u8; 20];
        sha[0] = 0x12;
        sha[1] = 0x34;
        buf.extend_from_slice(&sha);

        // CRC
        buf.extend_from_slice(&0x11223344u32.to_be_bytes());

        // Offset 100
        buf.extend_from_slice(&100u32.to_be_bytes());

        // Pack SHA
        buf.extend_from_slice(&[0xaa; 20]);
        // Idx SHA
        buf.extend_from_slice(&[0xbb; 20]);

        let parsed = PackIndex::parse("test.pack", &buf).unwrap();
        assert_eq!(parsed.object_count(), 1);

        let (off, len) = parsed.find_offset(&sha).unwrap();
        assert_eq!(off, 100);
        assert_eq!(len, 64 * 1024);

        let mut missing_sha = sha;
        missing_sha[1] = 0x99;
        assert!(parsed.find_offset(&missing_sha).is_none());
    }
}
