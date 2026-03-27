//! Ignore-pattern filtering for folder watching.
//!
//! Each shared folder may contain a `.murmurignore` file at its root (gitignore syntax).
//! A set of built-in default patterns is always applied on top.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tracing::warn;

/// Default ignore patterns applied to every folder.
const DEFAULT_PATTERNS: &[&str] = &[
    ".DS_Store",
    "Thumbs.db",
    "._*",
    "*.tmp",
    "*~",
    ".murmurignore",
    "*.conflict-*",
];

/// Ignore-pattern matcher for a single shared folder.
pub struct IgnoreFilter {
    gitignore: Gitignore,
}

impl IgnoreFilter {
    /// Build an ignore filter for the folder rooted at `folder_root`.
    ///
    /// Loads `.murmurignore` from the folder root (if present) and appends
    /// the built-in default patterns.
    pub fn new(folder_root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(folder_root);

        // Load .murmurignore if it exists.
        let ignore_file = folder_root.join(".murmurignore");
        if ignore_file.is_file()
            && let Some(err) = builder.add(&ignore_file)
        {
            warn!(
                path = %ignore_file.display(),
                error = %err,
                "failed to parse .murmurignore, skipping"
            );
        }

        // Always add default patterns.
        for pattern in DEFAULT_PATTERNS {
            if let Err(err) = builder.add_line(None, pattern) {
                warn!(pattern, error = %err, "bad default ignore pattern");
            }
        }

        let gitignore = builder.build().unwrap_or_else(|err| {
            warn!(error = %err, "failed to build ignore filter, using empty");
            Gitignore::empty()
        });

        Self { gitignore }
    }

    /// Returns `true` if `path` should be ignored (not synced).
    ///
    /// `path` must be relative to the folder root.
    /// `is_dir` indicates whether the path is a directory.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.gitignore
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_default_patterns_ds_store() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new(".DS_Store"), false));
    }

    #[test]
    fn test_default_patterns_thumbs_db() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new("Thumbs.db"), false));
    }

    #[test]
    fn test_default_patterns_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new("foo.tmp"), false));
        assert!(filter.is_ignored(Path::new("subdir/bar.tmp"), false));
    }

    #[test]
    fn test_default_patterns_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new("document.txt~"), false));
    }

    #[test]
    fn test_default_patterns_conflict_files() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new("report.conflict-a1b2c3-1711234567.docx"), false));
    }

    #[test]
    fn test_default_patterns_dot_underscore() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new("._somefile"), false));
    }

    #[test]
    fn test_default_patterns_murmurignore_itself() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new(".murmurignore"), false));
    }

    #[test]
    fn test_normal_file_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let filter = IgnoreFilter::new(dir.path());
        assert!(!filter.is_ignored(Path::new("photo.jpg"), false));
        assert!(!filter.is_ignored(Path::new("documents/report.pdf"), false));
    }

    #[test]
    fn test_custom_murmurignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".murmurignore"), "*.log\nbuild/\n").unwrap();

        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new("app.log"), false));
        assert!(filter.is_ignored(Path::new("build"), true));
        assert!(filter.is_ignored(Path::new("build/output.bin"), false));
        // Normal files still allowed
        assert!(!filter.is_ignored(Path::new("main.rs"), false));
    }

    #[test]
    fn test_custom_plus_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".murmurignore"), "*.log\n").unwrap();

        let filter = IgnoreFilter::new(dir.path());
        // Custom
        assert!(filter.is_ignored(Path::new("app.log"), false));
        // Default
        assert!(filter.is_ignored(Path::new(".DS_Store"), false));
    }

    #[test]
    fn test_no_murmurignore_file() {
        let dir = tempfile::tempdir().unwrap();
        // No .murmurignore — only defaults apply
        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new(".DS_Store"), false));
        assert!(!filter.is_ignored(Path::new("readme.md"), false));
    }

    #[test]
    fn test_negation_pattern() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".murmurignore"), "*.log\n!important.log\n").unwrap();

        let filter = IgnoreFilter::new(dir.path());
        assert!(filter.is_ignored(Path::new("debug.log"), false));
        assert!(!filter.is_ignored(Path::new("important.log"), false));
    }
}
