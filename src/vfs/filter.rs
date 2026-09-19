/// Checks if a requested filename is macOS/Finder metadata noise.
/// Returning NFS3ERR_NOENT immediately for these saves substantial network roundtrips.
pub fn is_apple_metadata(name: &str) -> bool {
    if name.starts_with("._") {
        return true;
    }
    match name {
        ".DS_Store"
        | ".localized"
        | ".Spotlight-V100"
        | ".Trashes"
        | ".fseventsd"
        | ".TemporaryItems"
        | ".VolumeIcon.icns" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apple_metadata_filter() {
        assert!(is_apple_metadata(".DS_Store"));
        assert!(is_apple_metadata("._Makefile"));
        assert!(is_apple_metadata(".Spotlight-V100"));
        assert!(is_apple_metadata(".Trashes"));
        assert!(!is_apple_metadata("Makefile"));
        assert!(!is_apple_metadata("main.rs"));
        assert!(!is_apple_metadata(".gitignore"));
    }
}
