use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Default)]
pub(crate) struct CleanupStats {
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    /// Total size of artifacts kept (in use)
    pub(crate) used_bytes: u64,
    pub(crate) per_crate: HashMap<String, CrateStat>,
    pub(crate) per_profile: HashMap<String, ProfileStat>,
    pub(crate) errors: HashMap<(String, String, String), anyhow::Error>,
    pub(crate) files_to_remove: Vec<FileToRemove>,
    /// Stale incremental compilation session directories to remove
    pub(crate) dirs_to_remove: Vec<DirToRemove>,
}

#[derive(Default, Clone)]
pub(crate) struct CrateStat {
    pub(crate) files: usize,
    pub(crate) bytes: u64,
}

#[derive(Default, Clone)]
pub(crate) struct ProfileStat {
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    /// Bytes in deps/ that are kept (in-use)
    pub(crate) used_bytes: u64,
    /// Total bytes in the entire profile directory (deps + incremental + build + …)
    pub(crate) total_dir_bytes: u64,
}

#[derive(Clone)]
pub(crate) struct FileToRemove {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) profile: String,
}

#[derive(Clone)]
pub(crate) struct DirToRemove {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    pub(crate) profile: String,
}

impl CleanupStats {
    pub(crate) fn merge_from(&mut self, other: CleanupStats) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.used_bytes += other.used_bytes;
        for (name, stat) in other.per_crate {
            let entry = self.per_crate.entry(name).or_default();
            entry.files += stat.files;
            entry.bytes += stat.bytes;
        }
        for (profile, stat) in other.per_profile {
            let entry = self.per_profile.entry(profile).or_default();
            entry.files += stat.files;
            entry.bytes += stat.bytes;
            entry.used_bytes += stat.used_bytes;
            entry.total_dir_bytes += stat.total_dir_bytes;
        }
        self.errors.extend(other.errors);
        self.files_to_remove.extend(other.files_to_remove);
        self.dirs_to_remove.extend(other.dirs_to_remove);
    }
}
