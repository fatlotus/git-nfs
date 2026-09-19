use anyhow::{anyhow, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeEntryMode {
    RegularFile,
    ExecutableFile,
    Directory,
    Symlink,
    Submodule,
    Other(u32),
}

impl TreeEntryMode {
    pub fn from_mode_str(s: &str) -> Self {
        match s {
            "100644" => TreeEntryMode::RegularFile,
            "100755" => TreeEntryMode::ExecutableFile,
            "40000" | "040000" => TreeEntryMode::Directory,
            "120000" => TreeEntryMode::Symlink,
            "160000" => TreeEntryMode::Submodule,
            other => {
                let m = u32::from_str_radix(other, 8).unwrap_or(0o100644);
                TreeEntryMode::Other(m)
            }
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, TreeEntryMode::Directory)
    }

    pub fn is_symlink(&self) -> bool {
        matches!(self, TreeEntryMode::Symlink)
    }

    pub fn posix_mode(&self) -> u32 {
        match self {
            TreeEntryMode::RegularFile => 0o100644,
            TreeEntryMode::ExecutableFile => 0o100755,
            TreeEntryMode::Directory => 0o040755,
            TreeEntryMode::Symlink => 0o120777,
            TreeEntryMode::Submodule => 0o040755,
            TreeEntryMode::Other(m) => *m,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitTreeEntry {
    pub mode: TreeEntryMode,
    pub name: String,
    pub oid: String,
}

#[derive(Debug, Clone, Default)]
pub struct GitTree {
    pub entries: Vec<GitTreeEntry>,
}

impl GitTree {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut pos = 0;
        let mut entries = Vec::new();

        while pos < data.len() {
            let space_pos = data[pos..]
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| anyhow!("Tree entry missing space delimiter after mode"))?;
            let mode_str = std::str::from_utf8(&data[pos..pos + space_pos])
                .map_err(|_| anyhow!("Invalid UTF-8 in tree entry mode"))?;
            let mode = TreeEntryMode::from_mode_str(mode_str);
            pos += space_pos + 1;

            let null_pos = data[pos..]
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| anyhow!("Tree entry missing null delimiter after filename"))?;
            let name = std::str::from_utf8(&data[pos..pos + null_pos])
                .map_err(|_| anyhow!("Invalid UTF-8 in tree entry name"))?
                .to_string();
            pos += null_pos + 1;

            if pos + 20 > data.len() {
                return Err(anyhow!("Tree entry truncated before 20-byte SHA-1"));
            }
            let oid = hex::encode(&data[pos..pos + 20]);
            pos += 20;

            entries.push(GitTreeEntry { mode, name, oid });
        }

        Ok(GitTree { entries })
    }

    #[allow(dead_code)]
    pub fn find(&self, name: &str) -> Option<&GitTreeEntry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_parser() {
        // Construct synthetic git tree data:
        // "100644 Makefile\0<20 bytes sha>"
        let mut raw = Vec::new();
        raw.extend_from_slice(b"100644 Makefile\0");
        raw.extend_from_slice(&[1u8; 20]);
        raw.extend_from_slice(b"40000 kernel\0");
        raw.extend_from_slice(&[2u8; 20]);

        let tree = GitTree::parse(&raw).unwrap();
        assert_eq!(tree.entries.len(), 2);
        assert_eq!(tree.entries[0].name, "Makefile");
        assert_eq!(tree.entries[0].mode, TreeEntryMode::RegularFile);
        assert_eq!(tree.entries[1].name, "kernel");
        assert_eq!(tree.entries[1].mode, TreeEntryMode::Directory);
        assert!(tree.entries[1].mode.is_dir());
    }
}
