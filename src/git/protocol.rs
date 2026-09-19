use anyhow::{anyhow, Result};

/// Formats a string into a Git pkt-line (4-byte hex length prefix + data + '\n').
pub fn encode_pkt_line(line: &str) -> Vec<u8> {
    let mut payload = line.as_bytes().to_vec();
    payload.push(b'\n');
    let total_len = payload.len() + 4;
    let hex_prefix = format!("{total_len:04x}");
    let mut out = hex_prefix.into_bytes();
    out.extend(payload);
    out
}

/// Flush packet: "0000"
pub fn flush_pkt() -> &'static [u8] {
    b"0000"
}

/// Delimiter packet: "0001"
pub fn delim_pkt() -> &'static [u8] {
    b"0001"
}

/// Parses sideband multiplexed stream (protocol v2 packfile output).
/// Channel 1: Packfile data
/// Channel 2: Progress message
/// Channel 3: Error message
pub fn extract_pack_from_sideband(data: &[u8]) -> Result<Vec<u8>> {
    let mut idx = 0;
    let mut pack = Vec::new();

    while idx < data.len() {
        if idx + 4 > data.len() {
            break;
        }
        let len_str = std::str::from_utf8(&data[idx..idx + 4])
            .map_err(|_| anyhow!("Invalid UTF-8 in pkt-line length"))?;
        let line_len = usize::from_str_radix(len_str, 16)
            .map_err(|_| anyhow!("Invalid hex length: {len_str}"))?;

        if line_len == 0 || line_len == 1 {
            idx += 4;
            continue;
        }

        if idx + line_len > data.len() {
            return Err(anyhow!(
                "Pkt-line length {line_len} exceeds available bytes {}",
                data.len() - idx
            ));
        }

        let chunk = &data[idx + 4..idx + line_len];
        idx += line_len;

        if !chunk.is_empty() {
            match chunk[0] {
                1 => {
                    // Channel 1: Packfile payload
                    pack.extend_from_slice(&chunk[1..]);
                }
                2 => {
                    // Channel 2: Progress message - ignore or debug log
                }
                3 => {
                    // Channel 3: Remote error message
                    let err_msg = String::from_utf8_lossy(&chunk[1..]);
                    return Err(anyhow!("Git remote error: {err_msg}"));
                }
                _ => {
                    // Some servers or dumb responses might not use sideband channel prefix
                    // if it is raw pack data starting with 'PACK'
                    if chunk.starts_with(b"PACK") {
                        pack.extend_from_slice(chunk);
                    }
                }
            }
        }
    }

    if pack.is_empty() {
        // Check if raw data itself starts with PACK
        if let Some(pack_pos) = data.windows(4).position(|w| w == b"PACK") {
            return Ok(data[pack_pos..].to_vec());
        }
        return Err(anyhow!("No packfile data found in server response"));
    }

    Ok(pack)
}

/// Parses raw pkt-lines from a response buffer into individual strings.
pub fn parse_pkt_lines(data: &[u8]) -> Result<Vec<String>> {
    let mut idx = 0;
    let mut lines = Vec::new();

    while idx < data.len() {
        if idx + 4 > data.len() {
            break;
        }
        let len_str = std::str::from_utf8(&data[idx..idx + 4])
            .map_err(|_| anyhow!("Invalid UTF-8 in pkt-line length"))?;
        let line_len = usize::from_str_radix(len_str, 16)
            .map_err(|_| anyhow!("Invalid hex length: {len_str}"))?;

        if line_len == 0 {
            // Flush packet
            idx += 4;
            continue;
        }
        if line_len == 1 {
            // Delim packet
            idx += 4;
            continue;
        }

        if idx + line_len > data.len() {
            break;
        }

        let chunk = &data[idx + 4..idx + line_len];
        idx += line_len;

        let s = String::from_utf8_lossy(chunk).trim_end_matches('\n').to_string();
        lines.push(s);
    }

    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_pkt_line() {
        let pkt = encode_pkt_line("command=ls-refs");
        assert_eq!(&pkt[..4], b"0014");
        assert_eq!(&pkt[4..], b"command=ls-refs\n");
    }

    #[test]
    fn test_parse_pkt_lines() {
        let mut buf = Vec::new();
        buf.extend(encode_pkt_line("hello"));
        buf.extend(flush_pkt());
        buf.extend(encode_pkt_line("world"));

        let lines = parse_pkt_lines(&buf).unwrap();
        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
    }
}
