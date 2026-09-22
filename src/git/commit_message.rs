use std::collections::{BTreeSet, HashMap};

use crate::vfs::filter::is_apple_metadata;

/// Auto-generates a Git commit message based on the set of modified repository-relative file paths.
///
/// Rules:
/// - Filters out macOS metadata files (e.g. `.DS_Store`, `._*`).
/// - If 0 files: "Empty commit: no files changed"
/// - If 1 to 3 files: "Update <file1>, <file2>, ..."
/// - If > 3 files: groups by top-level directory, picks the most active directories in the subject line,
///   and appends a directory summary in the commit body.
pub fn generate_commit_message(changed_paths: &[String]) -> String {
    let mut valid_paths = BTreeSet::new();

    for raw_path in changed_paths {
        let clean = raw_path.trim().trim_matches('/');
        if clean.is_empty() {
            continue;
        }

        // Check if any component in the path is Apple metadata
        let is_meta = clean
            .split('/')
            .any(|component| is_apple_metadata(component));

        if !is_meta {
            valid_paths.insert(clean.to_string());
        }
    }

    if valid_paths.is_empty() {
        return "Empty commit: no files changed".to_string();
    }

    let files: Vec<String> = valid_paths.into_iter().collect();
    let total_files = files.len();

    if total_files <= 3 {
        let file_list = files.join(", ");
        return format!("Update {file_list}");
    }

    // More than 3 files: group by top-level directory
    let mut dir_counts: HashMap<String, usize> = HashMap::new();
    for file in &files {
        let top_dir = if let Some(slash_idx) = file.find('/') {
            format!("{}/", &file[..slash_idx])
        } else {
            "root/".to_string()
        };
        *dir_counts.entry(top_dir).or_insert(0) += 1;
    }

    // Sort directories by count (descending), then by name (ascending)
    let mut sorted_dirs: Vec<(String, usize)> = dir_counts.into_iter().collect();
    sorted_dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let num_dirs = sorted_dirs.len();
    let subject = if num_dirs == 1 {
        let (dir_name, _) = &sorted_dirs[0];
        format!("Update {total_files} files in {dir_name}")
    } else if num_dirs <= 3 {
        let dir_names: Vec<&str> = sorted_dirs.iter().map(|(d, _)| d.as_str()).collect();
        format!("Update {total_files} files across {}", dir_names.join(", "))
    } else {
        let top2 = [sorted_dirs[0].0.as_str(), sorted_dirs[1].0.as_str()];
        let others_count = num_dirs - 2;
        let other_str = if others_count == 1 {
            "1 other directory".to_string()
        } else {
            format!("{others_count} other directories")
        };
        format!(
            "Update {total_files} files across {}, {} (and {other_str})",
            top2[0], top2[1]
        )
    };

    let mut body = String::from("\n\nChanged directories:\n");
    for (dir, count) in &sorted_dirs {
        let file_label = if *count == 1 { "file" } else { "files" };
        body.push_str(&format!("- {dir} ({count} {file_label})\n"));
    }

    format!("{subject}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_paths() {
        assert_eq!(
            generate_commit_message(&[]),
            "Empty commit: no files changed"
        );
        assert_eq!(
            generate_commit_message(&["   ".to_string(), "/".to_string()]),
            "Empty commit: no files changed"
        );
    }

    #[test]
    fn test_filters_apple_metadata() {
        let paths = vec![
            ".DS_Store".to_string(),
            "src/._foo.c".to_string(),
            ".Spotlight-V100/store.db".to_string(),
        ];
        assert_eq!(
            generate_commit_message(&paths),
            "Empty commit: no files changed"
        );
    }

    #[test]
    fn test_single_file() {
        let paths = vec!["Makefile".to_string()];
        assert_eq!(generate_commit_message(&paths), "Update Makefile");

        let paths_nested = vec!["src/main.rs".to_string()];
        assert_eq!(generate_commit_message(&paths_nested), "Update src/main.rs");
    }

    #[test]
    fn test_two_and_three_files() {
        let paths2 = vec!["b.txt".to_string(), "a.txt".to_string()];
        assert_eq!(generate_commit_message(&paths2), "Update a.txt, b.txt");

        let paths3 = vec![
            "src/main.rs".to_string(),
            "Cargo.toml".to_string(),
            "README.md".to_string(),
        ];
        assert_eq!(
            generate_commit_message(&paths3),
            "Update Cargo.toml, README.md, src/main.rs"
        );
    }

    #[test]
    fn test_many_files_single_dir() {
        let paths: Vec<String> = (1..=5).map(|i| format!("src/file_{i}.rs")).collect();
        let msg = generate_commit_message(&paths);
        assert!(msg.starts_with("Update 5 files in src/\n\nChanged directories:\n- src/ (5 files)"));
    }

    #[test]
    fn test_many_files_multiple_dirs() {
        let mut paths = vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string(),
            "tests/test1.rs".to_string(),
            "Cargo.toml".to_string(),
        ];
        // 5 files: 3 in src/, 1 in tests/, 1 in root/
        let msg = generate_commit_message(&paths);
        assert!(msg.starts_with("Update 5 files across src/, root/, tests/\n\nChanged directories:\n- src/ (3 files)"));

        // Add more directories to test > 3 directories
        paths.push("doc/readme.md".to_string());
        // 6 files across 4 dirs: src/ (3), doc/ (1), root/ (1), tests/ (1)
        let msg2 = generate_commit_message(&paths);
        assert!(msg2.starts_with("Update 6 files across src/"));
        assert!(msg2.contains("(and 2 other directories)"));
    }
}
