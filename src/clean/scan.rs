use std::path::Path;

#[derive(Clone, Copy, Default)]
pub(super) struct FolderSizeEstimate {
    pub(super) bytes: u64,
    pub(super) incomplete: bool,
}

/// Inspect metadata only, counting each hardlinked inode once on Unix. Symlinks
/// contribute their own allocation but are never followed into other folders.
pub(super) fn folder_size_estimate(root: &Path) -> FolderSizeEstimate {
    let mut estimate = FolderSizeEstimate::default();
    let mut pending = vec![root.to_path_buf()];
    #[cfg(unix)]
    let mut seen = std::collections::HashSet::new();
    while let Some(path) = pending.pop() {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            estimate.incomplete = true;
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if !seen.insert((metadata.dev(), metadata.ino())) {
                continue;
            }
            estimate.bytes = estimate
                .bytes
                .saturating_add(metadata.blocks().saturating_mul(512));
        }
        #[cfg(not(unix))]
        if metadata.is_file() {
            estimate.bytes = estimate.bytes.saturating_add(metadata.len());
        }
        if metadata.is_dir() {
            match std::fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => pending.push(entry.path()),
                            Err(_) => estimate.incomplete = true,
                        }
                    }
                }
                Err(_) => estimate.incomplete = true,
            }
        }
    }
    estimate
}

/// Disk usage of `dir` (hardlinks counted once, symlinks not followed).
pub(super) fn dir_size_bytes(dir: &Path) -> u64 {
    folder_size_estimate(dir).bytes
}

/// Stems (`crate-HASH`) in `deps_dir` that are the *current* final outputs.
///
/// Cargo hardlinks final outputs from `<profile>/` (and `<profile>/examples/`)
/// to `deps/`, so a deps file sharing an inode with a profile-level file is the
/// live one. Crates with no inode match (e.g. copied outputs) fall back to
/// their newest stem by mtime. Older hashes of the same crate are NOT
/// protected, so they can be reclaimed.
#[cfg(unix)]
pub(super) fn current_output_stems(deps_dir: &Path) -> std::collections::HashSet<String> {
    use std::collections::{HashMap, HashSet};
    use std::os::unix::fs::MetadataExt;
    use std::time::SystemTime;

    let mut stems = HashSet::new();
    let Some(profile_dir) = deps_dir.parent() else {
        return stems;
    };

    // (dev, ino) of every plain file in the profile dir and examples/
    let mut live_inodes = HashSet::new();
    let mut live_names = HashSet::new();
    for dir in [profile_dir.to_path_buf(), profile_dir.join("examples")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.is_file() {
                live_inodes.insert((m.dev(), m.ino()));
                live_names.insert(crate::crate_deps::crate_key(&e.path()).replace('-', "_"));
            }
        }
    }

    let mut matched_names = HashSet::new();
    let mut newest: HashMap<String, (SystemTime, String)> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(deps_dir) {
        for e in entries.flatten() {
            let path = e.path();
            let Some(stem) = artifact_stem(&path) else {
                continue;
            };
            let Ok(m) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !m.is_file() {
                continue;
            }
            let key = crate::crate_deps::crate_key(&path);
            if live_inodes.contains(&(m.dev(), m.ino())) {
                stems.insert(stem.clone());
                matched_names.insert(key.clone());
            }
            let mtime = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let slot = newest.entry(key).or_insert((mtime, stem.clone()));
            if mtime > slot.0 {
                *slot = (mtime, stem);
            }
        }
    }

    for name in live_names {
        if !matched_names.contains(&name)
            && let Some((_, stem)) = newest.get(&name)
        {
            stems.insert(stem.clone());
        }
    }
    stems
}

#[cfg(not(unix))]
pub(super) fn current_output_stems(_deps_dir: &Path) -> std::collections::HashSet<String> {
    std::collections::HashSet::new()
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

    #[cfg(unix)]
    #[test]
    fn current_output_stems_protects_only_live_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        let deps = profile.join("deps");
        fs::create_dir_all(&deps).unwrap();
        fs::write(deps.join("libapp-old1.rlib"), b"old").unwrap();
        fs::write(deps.join("libapp-live.rlib"), b"live").unwrap();
        fs::write(deps.join("app-live.o"), b"o").unwrap();
        fs::hard_link(deps.join("libapp-live.rlib"), profile.join("libapp.rlib")).unwrap();
        let stems = current_output_stems(&deps);
        assert!(stems.contains("app-live"));
        assert!(!stems.contains("app-old1"));
    }
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    #[test]
    fn folder_estimate_includes_nested_artifacts_and_ignores_other_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::write(profile.join("deps/first.rlib"), vec![1u8; 8192]).unwrap();
        let first = folder_size_estimate(&profile);
        assert!(first.bytes >= 8192);
        assert!(!first.incomplete);
        fs::create_dir_all(tmp.path().join("release")).unwrap();
        fs::write(tmp.path().join("release/other.rlib"), vec![1u8; 16384]).unwrap();
        assert_eq!(folder_size_estimate(&profile).bytes, first.bytes);
        fs::write(profile.join("deps/second.rlib"), vec![2u8; 8192]).unwrap();
        assert!(folder_size_estimate(&profile).bytes > first.bytes);
    }

    #[cfg(unix)]
    #[test]
    fn folder_estimate_does_not_double_count_hardlinks_or_follow_symlinks() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("debug");
        fs::create_dir(&profile).unwrap();
        fs::write(profile.join("artifact"), vec![1u8; 8192]).unwrap();
        fs::hard_link(profile.join("artifact"), profile.join("artifact-copy")).unwrap();
        fs::write(tmp.path().join("outside"), vec![2u8; 32768]).unwrap();
        symlink(tmp.path().join("outside"), profile.join("outside-link")).unwrap();
        symlink(&profile, profile.join("cycle")).unwrap();
        let expected: u64 = [
            &profile,
            &profile.join("artifact"),
            &profile.join("outside-link"),
            &profile.join("cycle"),
        ]
        .into_iter()
        .map(|p| fs::symlink_metadata(p).unwrap().blocks() * 512)
        .sum();
        let estimate = folder_size_estimate(&profile);
        assert_eq!(estimate.bytes, expected);
        assert!(!estimate.incomplete);
    }

    #[test]
    fn missing_folder_size_is_not_reported_as_a_complete_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(folder_size_estimate(&tmp.path().join("missing")).incomplete);
    }

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
