use std::path::Path;

/// Recursively sum the size of all files under `dir` (sync, no extra deps).
pub(super) fn dir_size_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                dir_size_bytes(&p)
            } else {
                p.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// Extract the `crate_name-HASH` stem from any artifact file:
/// - `libfoo-HASH.rlib`              → `foo-HASH`
/// - `libfoo-HASH.rmeta`             → `foo-HASH`
/// - `foo-HASH.d`                    → `foo-HASH`
/// - `foo-HASH.foo.cgu.00.rcgu.dwo`  → `foo-HASH`
/// - `foo-HASH.foo.cgu.00.rcgu.o`    → `foo-HASH`
pub(super) fn artifact_stem(path: &Path) -> Option<String> {
    let filename = path.file_name()?.to_str()?;
    // Strip "lib" prefix (rlib/rmeta files carry it, dwo/o/d don't)
    let without_lib = filename.strip_prefix("lib").unwrap_or(filename);
    // Take everything before the first dot
    let stem = without_lib.split_once('.').map_or(without_lib, |(s, _)| s);
    // Must contain '-' (crate name / hash separator) to be a valid artifact
    if stem.contains('-') {
        Some(stem.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    // ── artifact_stem ─────────────────────────────────────────────────────────

    #[test]
    fn artifact_stem_rlib() {
        assert_eq!(
            artifact_stem(Path::new("libserde-abc123.rlib")),
            Some("serde-abc123".to_string())
        );
    }

    #[test]
    fn artifact_stem_rmeta() {
        assert_eq!(
            artifact_stem(Path::new("libregex_automata-0b81c4f4.rmeta")),
            Some("regex_automata-0b81c4f4".to_string())
        );
    }

    #[test]
    fn artifact_stem_d_file_no_lib() {
        assert_eq!(
            artifact_stem(Path::new("cargo_clean-abc.d")),
            Some("cargo_clean-abc".to_string())
        );
    }

    #[test]
    fn artifact_stem_multi_ext() {
        // foo-HASH.foo.cgu.00.rcgu.dwo → foo-HASH
        assert_eq!(
            artifact_stem(Path::new("foo-HASH.foo.cgu.00.rcgu.dwo")),
            Some("foo-HASH".to_string())
        );
    }

    #[test]
    fn artifact_stem_no_hash_returns_none() {
        // No '-' in stem → not a valid artifact
        assert_eq!(artifact_stem(Path::new("libserde.rlib")), None);
    }

    #[test]
    fn artifact_stem_strips_lib_prefix() {
        // lib prefix should be stripped before checking for '-'
        let s = artifact_stem(Path::new("libfoo-abc.rlib")).unwrap();
        assert_eq!(s, "foo-abc");
        assert!(!s.starts_with("lib"));
    }

    // ── clean_incremental_dir ─────────────────────────────────────────────────

    /// Helper: create a directory and touch its mtime `offset` seconds in the past.
    fn make_session(base: &Path, name: &str, age_secs: u64) {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        // Write a dummy file so the dir has content
        fs::write(dir.join("data"), vec![0u8; 1024]).unwrap();
        // Set mtime to `age_secs` seconds ago
        let mtime = SystemTime::now() - Duration::from_secs(age_secs);
        filetime::set_file_mtime(&dir, filetime::FileTime::from_system_time(mtime)).ok(); // ignore if filetime crate unavailable; mtime ordering still works
    }

    #[tokio::test]
    async fn clean_incremental_keeps_newest_session() {
        let tmp = tempfile::tempdir().unwrap();
        let inc = tmp.path().join("incremental");
        fs::create_dir_all(&inc).unwrap();

        // Three sessions for "bevy_pbr", oldest → newest
        make_session(&inc, "bevy_pbr-1aaaaaaaaaaaa", 300); // oldest
        make_session(&inc, "bevy_pbr-2bbbbbbbbbbb", 200);
        make_session(&inc, "bevy_pbr-3ccccccccccc", 10); // newest

        // One session for "serde" (should not be removed)
        make_session(&inc, "serde-4ddddddddddd", 150);

        let stats = super::super::CleanCommand::clean_incremental_dir(tmp.path(), "debug")
            .await
            .unwrap();

        // Should mark 2 stale bevy_pbr sessions for removal (keep the newest)
        assert_eq!(
            stats.dirs_to_remove.len(),
            2,
            "dirs_to_remove: {:?}",
            stats
                .dirs_to_remove
                .iter()
                .map(|d| &d.path)
                .collect::<Vec<_>>()
        );

        // Newest bevy_pbr should NOT be in the list
        let removed_names: Vec<_> = stats
            .dirs_to_remove
            .iter()
            .map(|d| d.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert!(
            !removed_names.iter().any(|n| n.contains("3ccccccccccc")),
            "newest session should be kept, got: {removed_names:?}"
        );
        assert!(
            removed_names.iter().any(|n| n.contains("1aaaaaaaaaaaa")),
            "oldest session should be removed"
        );
        assert!(
            removed_names.iter().any(|n| n.contains("2bbbbbbbbbbb")),
            "middle session should be removed"
        );

        // Single-session crate should never be touched
        assert!(
            !removed_names.iter().any(|n| n.contains("serde")),
            "single-session crate should not be removed"
        );
    }

    #[tokio::test]
    async fn clean_incremental_single_session_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let inc = tmp.path().join("incremental");
        fs::create_dir_all(&inc).unwrap();
        make_session(&inc, "my_crate-1aaaaaaaaaaaa", 100);

        let stats = super::super::CleanCommand::clean_incremental_dir(tmp.path(), "debug")
            .await
            .unwrap();

        assert!(stats.dirs_to_remove.is_empty());
        assert_eq!(stats.bytes, 0);
    }

    #[tokio::test]
    async fn clean_incremental_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // No incremental/ dir at all
        let stats = super::super::CleanCommand::clean_incremental_dir(tmp.path(), "debug")
            .await
            .unwrap();
        assert!(stats.dirs_to_remove.is_empty());
    }
}
