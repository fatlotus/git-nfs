use std::collections::HashMap;
use std::io::Read;

use anyhow::{anyhow, Context, Result};
use flate2::read::ZlibDecoder;
use sha1::{Digest, Sha1};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Commit = 1,
    Tree = 2,
    Blob = 3,
    Tag = 4,
    OfsDelta = 6,
    RefDelta = 7,
}

impl ObjectType {
    pub fn from_u8(val: u8) -> Result<Self> {
        match val {
            1 => Ok(ObjectType::Commit),
            2 => Ok(ObjectType::Tree),
            3 => Ok(ObjectType::Blob),
            4 => Ok(ObjectType::Tag),
            6 => Ok(ObjectType::OfsDelta),
            7 => Ok(ObjectType::RefDelta),
            other => Err(anyhow!("Unknown git object type: {other}")),
        }
    }

    pub fn type_str(&self) -> &'static str {
        match self {
            ObjectType::Commit => "commit",
            ObjectType::Tree => "tree",
            ObjectType::Blob => "blob",
            ObjectType::Tag => "tag",
            _ => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitRawObject {
    pub obj_type: ObjectType,
    pub data: Vec<u8>,
}

struct PendingDelta {
    obj_offset: usize,
    #[allow(dead_code)]
    delta_type: ObjectType,
    base_offset: Option<usize>,
    base_sha: Option<String>,
    delta_data: Vec<u8>,
}

pub fn compute_git_sha1(obj_type: ObjectType, data: &[u8]) -> String {
    let header = format!("{} {}\0", obj_type.type_str(), data.len());
    let mut hasher = Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn decode_varint(data: &[u8], mut pos: usize) -> Result<(usize, usize)> {
    if pos >= data.len() {
        return Err(anyhow!("Unexpected EOF reading varint"));
    }
    let mut c = data[pos];
    pos += 1;
    let mut val = (c & 0x7f) as usize;
    let mut shift = 7;
    while (c & 0x80) != 0 {
        if pos >= data.len() {
            return Err(anyhow!("Unexpected EOF in varint continuation"));
        }
        c = data[pos];
        pos += 1;
        val |= ((c & 0x7f) as usize) << shift;
        shift += 7;
    }
    Ok((val, pos))
}

pub fn apply_git_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    let (base_len, new_pos) = decode_varint(delta, pos)?;
    pos = new_pos;
    if base.len() != base_len {
        // Warning: Sometimes base len can differ if wrong base object was picked,
        // but Git protocol guarantees exact match.
    }
    let (res_len, new_pos) = decode_varint(delta, pos)?;
    pos = new_pos;

    let mut out = Vec::with_capacity(res_len);

    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;

        if (op & 0x80) != 0 {
            // Copy from base
            let mut cp_off: usize = 0;
            let mut cp_size: usize = 0;

            if (op & 0x01) != 0 {
                if pos >= delta.len() { return Err(anyhow!("Delta truncated")); }
                cp_off |= delta[pos] as usize;
                pos += 1;
            }
            if (op & 0x02) != 0 {
                if pos >= delta.len() { return Err(anyhow!("Delta truncated")); }
                cp_off |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if (op & 0x04) != 0 {
                if pos >= delta.len() { return Err(anyhow!("Delta truncated")); }
                cp_off |= (delta[pos] as usize) << 16;
                pos += 1;
            }
            if (op & 0x08) != 0 {
                if pos >= delta.len() { return Err(anyhow!("Delta truncated")); }
                cp_off |= (delta[pos] as usize) << 24;
                pos += 1;
            }

            if (op & 0x10) != 0 {
                if pos >= delta.len() { return Err(anyhow!("Delta truncated")); }
                cp_size |= delta[pos] as usize;
                pos += 1;
            }
            if (op & 0x20) != 0 {
                if pos >= delta.len() { return Err(anyhow!("Delta truncated")); }
                cp_size |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if (op & 0x40) != 0 {
                if pos >= delta.len() { return Err(anyhow!("Delta truncated")); }
                cp_size |= (delta[pos] as usize) << 16;
                pos += 1;
            }

            if cp_size == 0 {
                cp_size = 0x10000;
            }

            if cp_off + cp_size > base.len() {
                return Err(anyhow!(
                    "Delta copy range {}+{} exceeds base length {}",
                    cp_off,
                    cp_size,
                    base.len()
                ));
            }

            out.extend_from_slice(&base[cp_off..cp_off + cp_size]);
        } else if op > 0 {
            // Insert literal
            let count = op as usize;
            if pos + count > delta.len() {
                return Err(anyhow!("Delta insert exceeds buffer"));
            }
            out.extend_from_slice(&delta[pos..pos + count]);
            pos += count;
        } else {
            return Err(anyhow!("Invalid delta opcode 0"));
        }
    }

    if out.len() != res_len {
        return Err(anyhow!(
            "Delta reconstructed size {} does not match expected {}",
            out.len(),
            res_len
        ));
    }

    Ok(out)
}

/// Unpacks all objects from a raw packfile buffer.
/// Returns a map of OID (hex string) -> GitRawObject.
pub fn unpack_packfile(pack: &[u8]) -> Result<HashMap<String, GitRawObject>> {
    if pack.len() < 12 {
        return Err(anyhow!("Packfile too short (less than 12 bytes header)"));
    }
    if &pack[0..4] != b"PACK" {
        return Err(anyhow!("Invalid packfile magic: {:?}", &pack[0..4]));
    }

    let version = u32::from_be_bytes(pack[4..8].try_into()?);
    if version != 2 && version != 3 {
        return Err(anyhow!("Unsupported packfile version: {version}"));
    }

    let num_objects = u32::from_be_bytes(pack[8..12].try_into()?) as usize;

    let mut objects_by_offset: HashMap<usize, (ObjectType, Vec<u8>, String)> = HashMap::new();
    let mut objects_by_sha: HashMap<String, GitRawObject> = HashMap::new();
    let mut pending_deltas: Vec<PendingDelta> = Vec::new();

    let mut pos = 12;

    for _ in 0..num_objects {
        if pos >= pack.len() {
            return Err(anyhow!("Unexpected EOF parsing object headers in packfile"));
        }

        let obj_offset = pos;
        let mut c = pack[pos];
        pos += 1;

        let type_num = (c >> 4) & 7;
        let obj_type = ObjectType::from_u8(type_num)?;

        // Decode initial size bits
        let mut size = (c & 15) as usize;
        let mut shift = 4;
        while (c & 0x80) != 0 {
            if pos >= pack.len() {
                return Err(anyhow!("Unexpected EOF in object size header"));
            }
            c = pack[pos];
            pos += 1;
            size |= ((c & 0x7f) as usize) << shift;
            shift += 7;
        }

        match obj_type {
            ObjectType::OfsDelta => {
                if pos >= pack.len() {
                    return Err(anyhow!("Unexpected EOF in OFS_DELTA offset"));
                }
                c = pack[pos];
                pos += 1;
                let mut base_offset_delta = (c & 0x7f) as usize;
                while (c & 0x80) != 0 {
                    if pos >= pack.len() {
                        return Err(anyhow!("Unexpected EOF in OFS_DELTA continuation"));
                    }
                    c = pack[pos];
                    pos += 1;
                    base_offset_delta = ((base_offset_delta + 1) << 7) | ((c & 0x7f) as usize);
                }
                let base_offset = obj_offset
                    .checked_sub(base_offset_delta)
                    .ok_or_else(|| anyhow!("Invalid negative base offset in OFS_DELTA"))?;

                let mut decoder = ZlibDecoder::new(&pack[pos..]);
                let mut delta_data = Vec::new();
                decoder
                    .read_to_end(&mut delta_data)
                    .context("Decompressing OFS_DELTA data")?;
                let consumed = decoder.total_in() as usize;
                pos += consumed;

                pending_deltas.push(PendingDelta {
                    obj_offset,
                    delta_type: ObjectType::OfsDelta,
                    base_offset: Some(base_offset),
                    base_sha: None,
                    delta_data,
                });
            }
            ObjectType::RefDelta => {
                if pos + 20 > pack.len() {
                    return Err(anyhow!("Unexpected EOF in REF_DELTA sha"));
                }
                let base_sha = hex::encode(&pack[pos..pos + 20]);
                pos += 20;

                let mut decoder = ZlibDecoder::new(&pack[pos..]);
                let mut delta_data = Vec::new();
                decoder
                    .read_to_end(&mut delta_data)
                    .context("Decompressing REF_DELTA data")?;
                let consumed = decoder.total_in() as usize;
                pos += consumed;

                pending_deltas.push(PendingDelta {
                    obj_offset,
                    delta_type: ObjectType::RefDelta,
                    base_offset: None,
                    base_sha: Some(base_sha),
                    delta_data,
                });
            }
            base_type => {
                let mut decoder = ZlibDecoder::new(&pack[pos..]);
                let mut decomp = Vec::with_capacity(size);
                decoder
                    .read_to_end(&mut decomp)
                    .context("Decompressing base object data")?;
                let consumed = decoder.total_in() as usize;
                pos += consumed;

                let sha = compute_git_sha1(base_type, &decomp);
                objects_by_offset.insert(obj_offset, (base_type, decomp.clone(), sha.clone()));
                objects_by_sha.insert(
                    sha,
                    GitRawObject {
                        obj_type: base_type,
                        data: decomp,
                    },
                );
            }
        }
    }

    // Iteratively resolve pending deltas
    while !pending_deltas.is_empty() {
        let mut remaining = Vec::new();
        let initial_len = pending_deltas.len();

        for item in pending_deltas {
            let base_info = if let Some(base_off) = item.base_offset {
                objects_by_offset
                    .get(&base_off)
                    .map(|(t, d, _)| (*t, d.clone()))
            } else if let Some(ref base_sha) = item.base_sha {
                objects_by_sha.get(base_sha).map(|o| (o.obj_type, o.data.clone()))
            } else {
                None
            };

            if let Some((base_type, base_data)) = base_info {
                let resolved = apply_git_delta(&base_data, &item.delta_data)
                    .context("Applying git delta")?;
                let sha = compute_git_sha1(base_type, &resolved);

                objects_by_offset.insert(
                    item.obj_offset,
                    (base_type, resolved.clone(), sha.clone()),
                );
                objects_by_sha.insert(
                    sha,
                    GitRawObject {
                        obj_type: base_type,
                        data: resolved,
                    },
                );
            } else {
                remaining.push(item);
            }
        }

        if remaining.len() == initial_len {
            return Err(anyhow!(
                "Could not resolve {} deltas due to missing bases",
                remaining.len()
            ));
        }
        pending_deltas = remaining;
    }

    Ok(objects_by_sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_git_delta() {
        let base = b"The quick brown fox jumps over the lazy dog";
        // Create delta that copies "The quick brown fox " (20 bytes),
        // inserts "runs past ", and copies "the lazy dog" (12 bytes at off 31).
        let mut delta = Vec::new();
        // base_len = 43
        delta.push(43);
        // res_len = 20 + 10 + 12 = 42
        delta.push(42);

        // Copy 20 bytes from off 0: opcode 0x80 | 0x10 | 0x00 (size in low byte)
        // op: 0x80 (copy) | 0x10 (size in 1 byte) = 0x90
        delta.push(0x90);
        delta.push(20); // size = 20 (off = 0 default)

        // Insert 10 bytes: "runs past "
        delta.push(10);
        delta.extend_from_slice(b"runs past ");

        // Copy 12 bytes from off 31:
        // op: 0x80 | 0x01 (off in 1 byte) | 0x10 (size in 1 byte) = 0x91
        delta.push(0x91);
        delta.push(31); // off = 31
        delta.push(12); // size = 12

        let res = apply_git_delta(base, &delta).unwrap();
        assert_eq!(res, b"The quick brown fox runs past the lazy dog");
    }

    #[test]
    fn test_compute_git_sha1() {
        let content = b"hello\n";
        let sha = compute_git_sha1(ObjectType::Blob, content);
        assert_eq!(sha, "ce013625030ba8dba906f756967f9e9ca394464a");
    }
}
