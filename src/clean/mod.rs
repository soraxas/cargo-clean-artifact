use std::{
    collections::HashMap,
    env,
    io::IsTerminal,
    path::{Path, PathBuf},
};

use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::ArgAction;
use clap::{Args, ValueHint};
use futures::future::try_join_all;
use tokio::fs;

use indicatif::{ProgressBar, ProgressStyle};

use crate::crate_deps::{crate_key, paint};
use crate::trace_parser::TraceParser;

mod display;
mod prompt;
mod scan;
mod stats;

use display::{
    print_detailed_summary, print_dry_run_summary, print_profile_breakdown, print_removal_summary,
};
use prompt::{RemovalSelection, prompt_step_by_step, select_command_interactive};
use scan::{artifact_stem, dir_size_bytes};
use stats::{CleanupStats, DirToRemove, FileToRemove};

/// Clean unused, old project files.
///
/// 1. This removes
///
///  - the unused files in `target` directory.
#[derive(Debug, Clone, Args)]
pub(crate) struct CleanCommand {
    /// Actually remove files (dry-run is the default).
    #[clap(short = 'y', long = "yes")]
    yes: bool,

    /// Force dry-run mode (default behavior).
    #[clap(long, action = ArgAction::SetTrue)]
    dry_run: bool,

    /// The directory to clean.
    ///
    #[clap(
        value_hint = ValueHint::DirPath,
        default_value = ".",
        value_name = "DIR"
    )]
    dir: PathBuf,

    /// Allow cleaning even when CARGO_TARGET_DIR is set (shared/global cache).
    #[clap(long, action = ArgAction::SetTrue)]
    allow_shared_target_dir: bool,

    /// Build command to trace (e.g. "cargo build --release --target wasm32-unknown-unknown").
    /// Passed to `sh -c`, so quoting and spaces work as normal.
    #[clap(short = 'c', long = "command", value_name = "COMMAND")]
    custom_command: Option<String>,

    /// Enable verbose output (debug logging).
    #[clap(short = 'v', long = "verbose")]
    verbose: bool,

    /// Print the top N largest in-use artifacts found during tracing.
    /// Set to 0 to disable.
    #[clap(
        long = "trace-stats",
        short = 'n',
        value_name = "N",
        default_value = "5"
    )]
    trace_stats: usize,
}

impl CleanCommand {
    /// Check if verbose mode is enabled
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }

    /// Clean up `target` of cargo.
    ///
    /// We only remove build outputs for outdated dependencies.
    pub(super) async fn remove_unused_files_of_cargo(
        &self,
        git_dir: &Path,
        cmd: &str,
    ) -> Result<CleanupStats> {
        let metadata = MetadataCommand::new().current_dir(git_dir).exec();

        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(e) => {
                eprintln!("⚠️  Warning: Not a cargo project or failed to read metadata");
                eprintln!("   Directory: {}", git_dir.display());
                log::debug!("Metadata error: {}", e);
                return Ok(CleanupStats::default());
            }
        };

        let target_dir = metadata.target_directory.as_std_path().to_path_buf();
        log::debug!("Target directory: {}", target_dir.display());

        if !target_dir.exists() {
            eprintln!(
                "⚠️  Warning: Target directory does not exist: {}",
                target_dir.display()
            );
            eprintln!("   Run your build command first to generate build artifacts.");
            return Ok(CleanupStats::default());
        }

        self.remove_unused_files_with_trace(git_dir, &target_dir, cmd)
            .await
    }

    /// Remove unused files using cargo trace mode
    async fn remove_unused_files_with_trace(
        &self,
        project_dir: &Path,
        target_dir: &Path,
        cmd: &str,
    ) -> Result<CleanupStats> {
        let parser = TraceParser::new(target_dir.to_path_buf());
        let trace_result = parser
            .trace(project_dir, cmd)
            .await
            .context("Failed to trace build command")?;

        // Derive all deps/ directories to scan from the trace result paths.
        // This automatically handles cross-compilation targets like
        // target/wasm32-unknown-unknown/wasm-dev/deps/.
        let mut scan_dirs: Vec<(PathBuf, String)> = Vec::new(); // (path, display_profile)
        for artifact in &trace_result.used_artifacts {
            if let Some(parent) = artifact.parent()
                && parent.file_name().is_some_and(|n| n == "deps")
                && parent.starts_with(target_dir)
                && !scan_dirs.iter().any(|(d, _)| d == parent)
            {
                // Display name: strip target_dir prefix and trailing "/deps"
                let display = parent
                    .strip_prefix(target_dir)
                    .ok()
                    .and_then(|p| p.parent()) // drop "deps" component
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string());
                scan_dirs.push((parent.to_path_buf(), display));
            }
        }

        log::debug!("Scanning {} deps directories", scan_dirs.len());
        for (dir, name) in &scan_dirs {
            log::debug!("  {} ({})", name, dir.display());
        }

        // Always show which profiles were observed in the trace
        if scan_dirs.is_empty() {
            println!("📂 Build profiles: \x1b[2m(none detected)\x1b[0m");
        } else {
            let profile_list: Vec<&str> = scan_dirs.iter().map(|(_, p)| p.as_str()).collect();
            println!(
                "📂 Build profiles: \x1b[1;36m{}\x1b[0m",
                profile_list.join("\x1b[0m, \x1b[1;36m")
            );
        }
        println!();

        // in-use artifact breakdown (always shown unless --trace-stats 0)
        if self.trace_stats > 0 {
            let n = self.trace_stats;
            let mut sized: Vec<(PathBuf, u64)> = trace_result
                .used_artifacts
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok().map(|m| (p.clone(), m.len())))
                .collect();
            sized.sort_by(|a, b| b.1.cmp(&a.1));
            let total = sized.len();
            let shown = n.min(total);
            println!(
                "\x1b[1;33m📦 Top {} in-use artifacts\x1b[0m \x1b[2m({} total):\x1b[0m",
                shown, total
            );
            for (i, (path, size)) in sized.iter().take(n).enumerate() {
                // Derive profile and filename separately
                let rel = path.strip_prefix(target_dir).ok();
                let profile = rel
                    .and_then(|r| r.parent()) // strip filename
                    .and_then(|r| r.parent()) // strip "deps"
                    .map(|r| r.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                let users: Vec<&str> = trace_result
                    .used_by
                    .get(path)
                    .map(|s| {
                        let mut v: Vec<&str> = s.iter().map(|x| x.as_str()).collect();
                        v.sort();
                        v
                    })
                    .unwrap_or_default();
                let used_by = if users.is_empty() {
                    None
                } else {
                    Some(users.as_slice())
                };
                println!(
                    "{}",
                    crate::theme::format_artifact_line(
                        i + 1,
                        &profile,
                        filename,
                        *size,
                        used_by,
                        &crate::theme::IN_USE
                    )
                );
            }
            if total > n {
                println!(
                    "{}",
                    crate::theme::format_more_line(total - n, "in-use files")
                );
            }
            println!();
        }

        let mut stats = CleanupStats::default();
        let mut found_any_profile = false;

        for (deps_dir, display_profile) in &scan_dirs {
            if !deps_dir.exists() {
                log::debug!("Profile directory does not exist: {}", deps_dir.display());
                continue;
            }

            found_any_profile = true;

            // Total size of the entire profile dir (deps + incremental + build + …)
            let total_dir_bytes = deps_dir.parent().map(dir_size_bytes).unwrap_or(0);

            let mut profile_stats = self
                .clean_with_trace_result(deps_dir, &trace_result.used_artifacts, display_profile)
                .await
                .context(format!("Failed to clean profile: {display_profile}"))?;

            // Attach total dir size to this profile's stat entry
            profile_stats
                .per_profile
                .entry(display_profile.clone())
                .or_default()
                .total_dir_bytes = total_dir_bytes;

            // Clean stale incremental sessions for this profile
            if let Some(profile_dir) = deps_dir.parent() {
                match CleanCommand::clean_incremental_dir(profile_dir, display_profile).await {
                    Ok(inc_stats) => profile_stats.merge_from(inc_stats),
                    Err(e) => log::warn!("Failed to scan incremental dir: {e}"),
                }
            }

            stats.merge_from(profile_stats);
        }

        if !found_any_profile {
            eprintln!("⚠️  Warning: No traced artifact directories found.");
            eprintln!(
                "   Make sure your build command produces output in the cargo target directory."
            );
            eprintln!("   Target directory: {}", target_dir.display());
        }

        Ok(stats)
    }

    /// Clean artifacts in a deps directory based on trace results.
    /// Uses stem-based grouping: all files sharing a `crate-HASH` stem with a
    /// used `.rlib`/`.rmeta` are kept. This catches `.dwo`, `.o`, `.d`, etc.
    ///
    /// Additionally protects files whose crate name matches a current build output
    /// in the parent profile directory (the final binary / library / wasm).
    async fn clean_with_trace_result(
        &self,
        deps_dir: &Path,
        used_artifacts: &std::collections::HashSet<PathBuf>,
        profile: &str,
    ) -> Result<CleanupStats> {
        // Build the set of used stems from artifacts that live in this deps dir
        let mut used_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
        for artifact in used_artifacts {
            if artifact.parent() == Some(deps_dir)
                && let Some(stem) = artifact_stem(artifact)
            {
                used_stems.insert(stem);
            }
        }

        // Build a set of current output crate names from files directly in the
        // parent profile directory (e.g. target/release/).  Files there are the
        // final build outputs — keep their corresponding deps/ intermediates.
        let mut protected_crate_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if let Some(profile_dir) = deps_dir.parent() {
            if let Ok(mut profile_entries) = fs::read_dir(profile_dir).await {
                while let Some(pe) = profile_entries.next_entry().await.ok().flatten() {
                    let p = pe.path();
                    if p.is_file()
                        && let Some(name) = p.file_stem().and_then(|s| s.to_str())
                    {
                        let name = name.strip_prefix("lib").unwrap_or(name);
                        // Normalize hyphens (final binary names use them; artifacts use _)
                        let normalized = name.replace('-', "_");
                        protected_crate_names.insert(normalized);
                    }
                }
            }
            log::debug!(
                "Protected crate names from {}: {:?}",
                profile_dir.display(),
                protected_crate_names
            );
        }

        let mut stats = CleanupStats::default();
        let mut entries = fs::read_dir(deps_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            // Derive stem; skip files with no recognisable artifact stem
            let stem = match artifact_stem(&path) {
                Some(s) => s,
                None => continue,
            };

            // Keep any file sharing a stem with a traced artifact
            if used_stems.contains(&stem) {
                let sz = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
                stats.used_bytes += sz;
                stats
                    .per_profile
                    .entry(profile.to_string())
                    .or_default()
                    .used_bytes += sz;
                continue;
            }

            // Keep any file whose crate name matches a current build output
            // (the root artifact is not in the trace since nothing depends on it)
            if protected_crate_names.contains(&crate_key(&path)) {
                let sz = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
                stats.used_bytes += sz;
                stats
                    .per_profile
                    .entry(profile.to_string())
                    .or_default()
                    .used_bytes += sz;
                continue;
            }

            // Unused – mark for removal
            let size = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
            let ck = crate_key(&path);

            stats.files_to_remove.push(FileToRemove {
                path: path.clone(),
                size,
                profile: profile.to_string(),
            });
            stats.files += 1;
            stats.bytes += size;
            stats.per_crate.entry(ck).or_default().files += 1;
            stats.per_crate.entry(crate_key(&path)).or_default().bytes += size;
            stats
                .per_profile
                .entry(profile.to_string())
                .or_default()
                .files += 1;
            stats
                .per_profile
                .entry(profile.to_string())
                .or_default()
                .bytes += size;
        }

        Ok(stats)
    }

    /// Scan `<profile_dir>/incremental/` and mark stale sessions for removal.
    ///
    /// Cargo stores incremental data in directories named `<crate_name>-<session_hash>/`.
    /// Multiple stale sessions accumulate over time. We keep only the most-recently
    /// modified session per crate name and mark the rest for removal.
    async fn clean_incremental_dir(profile_dir: &Path, profile: &str) -> Result<CleanupStats> {
        let incremental_dir = profile_dir.join("incremental");
        let mut stats = CleanupStats::default();

        if !incremental_dir.exists() {
            return Ok(stats);
        }

        // Collect all subdirectory entries with their mtime
        let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        let mut dir_iter = fs::read_dir(&incremental_dir).await?;
        while let Some(entry) = dir_iter.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                let mtime = fs::metadata(&path)
                    .await
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                entries.push((path, mtime));
            }
        }

        // Group by crate name (split on last '-' to separate session hash)
        let mut by_crate: HashMap<String, Vec<(PathBuf, std::time::SystemTime)>> = HashMap::new();
        for (path, mtime) in entries {
            let dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            // Split on last hyphen: <crate_name>-<session_hash>
            let crate_name = match dir_name.rfind('-') {
                Some(pos) => dir_name[..pos].to_string(),
                None => dir_name,
            };
            by_crate.entry(crate_name).or_default().push((path, mtime));
        }

        // For each crate, keep the newest session, mark the rest for removal
        for (_crate_name, mut sessions) in by_crate {
            if sessions.len() <= 1 {
                continue;
            }
            // Sort newest first
            sessions.sort_by(|a, b| b.1.cmp(&a.1));
            // Keep index 0 (newest), remove the rest
            for (path, _) in sessions.into_iter().skip(1) {
                let size = dir_size_bytes(&path);
                stats.dirs_to_remove.push(DirToRemove {
                    path,
                    size,
                    profile: profile.to_string(),
                });
                stats.files += 1;
                stats.bytes += size;
                stats
                    .per_profile
                    .entry(profile.to_string())
                    .or_default()
                    .files += 1;
                stats
                    .per_profile
                    .entry(profile.to_string())
                    .or_default()
                    .bytes += size;
            }
        }

        Ok(stats)
    }

    async fn actually_remove_files(
        &self,
        stats: &CleanupStats,
        sel: &RemovalSelection,
    ) -> Result<CleanupStats> {
        let mut removal_stats = CleanupStats::default();

        let total = if sel.remove_files {
            stats.files_to_remove.len()
        } else {
            0
        } + if sel.remove_dirs {
            stats.dirs_to_remove.len()
        } else {
            0
        };
        let pb = ProgressBar::new(total as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} items ({bytes_per_sec}) {msg}")
                .unwrap()
                .progress_chars("=>-"),
        );
        pb.set_message("Removing...");

        for file_info in stats.files_to_remove.iter().filter(|_| sel.remove_files) {
            let ck = crate_key(&file_info.path);
            match fs::remove_file(&file_info.path).await {
                Ok(_) => {
                    removal_stats.files += 1;
                    removal_stats.bytes += file_info.size;
                    let entry = removal_stats.per_crate.entry(ck.clone()).or_default();
                    entry.files += 1;
                    entry.bytes += file_info.size;
                    let profile_entry = removal_stats
                        .per_profile
                        .entry(file_info.profile.clone())
                        .or_default();
                    profile_entry.files += 1;
                    profile_entry.bytes += file_info.size;
                }
                Err(e) => {
                    removal_stats.errors.insert(
                        (
                            ck.clone(),
                            file_info.profile.clone(),
                            file_info.path.display().to_string(),
                        ),
                        e.into(),
                    );
                }
            }
            pb.inc(1);
        }

        for dir_info in stats.dirs_to_remove.iter().filter(|_| sel.remove_dirs) {
            match fs::remove_dir_all(&dir_info.path).await {
                Ok(_) => {
                    removal_stats.files += 1;
                    removal_stats.bytes += dir_info.size;
                    let profile_entry = removal_stats
                        .per_profile
                        .entry(dir_info.profile.clone())
                        .or_default();
                    profile_entry.files += 1;
                    profile_entry.bytes += dir_info.size;
                }
                Err(e) => {
                    removal_stats.errors.insert(
                        (
                            String::from("incremental"),
                            dir_info.profile.clone(),
                            dir_info.path.display().to_string(),
                        ),
                        e.into(),
                    );
                }
            }
            pb.inc(1);
        }

        pb.finish_and_clear();

        Ok(removal_stats)
    }

    pub async fn run(self) -> Result<()> {
        // Resolve the build command (interactive picker when -c is absent on a TTY)
        let resolved_cmd: Option<String> = if self.custom_command.is_some() {
            self.custom_command.clone()
        } else {
            match select_command_interactive()? {
                Some(cmd) => Some(cmd),
                None => {
                    eprintln!(
                        "\x1b[1;31merror\x1b[0m: the following required arguments were not provided:"
                    );
                    eprintln!("  \x1b[32m-c, --command <COMMAND>\x1b[0m");
                    eprintln!();
                    eprintln!("Examples:");
                    eprintln!("  cargo-clean-artifact -c 'cargo build'");
                    eprintln!("  cargo-clean-artifact -c 'cargo build --release'");
                    eprintln!(
                        "  cargo-clean-artifact -c 'cargo build -F my_feat --target wasm32-unknown-unknown'"
                    );
                    eprintln!("  cargo-clean-artifact -c 'trunk build'");
                    eprintln!("  cargo-clean-artifact -c 'mise run my-build-task'");
                    eprintln!();
                    eprintln!("For more information, try '\x1b[1m--help\x1b[0m'.");
                    std::process::exit(2);
                }
            }
        };
        let build_cmd = resolved_cmd.expect("command must be set");

        if let Ok(target_dir) = env::var("CARGO_TARGET_DIR") {
            let color = std::io::stderr().is_terminal();
            let warn_style = Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold();
            let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));
            let block_style = Style::new().fg_color(Some(AnsiColor::Magenta.into()));
            let stop_style = Style::new().fg_color(Some(AnsiColor::Red.into())).bold();

            eprintln!(
                "{} {}",
                paint(color, "Warning:", warn_style),
                paint(
                    color,
                    format!("CARGO_TARGET_DIR is set to {target_dir}"),
                    accent_style
                )
            );
            eprintln!(
                "{}",
                paint(
                    color,
                    "Cleaning a shared/global target may remove artifacts of other workspaces.",
                    block_style
                )
            );
            if self.allow_shared_target_dir {
                eprintln!(
                    "{} {}",
                    paint(color, "Proceeding because", accent_style),
                    paint(color, "--allow-shared-target-dir was set.", warn_style)
                );
            } else {
                eprintln!(
                    "{}",
                    paint(
                        color,
                        "Refusing to proceed without --allow-shared-target-dir.",
                        stop_style
                    )
                );
                anyhow::bail!(
                    "CARGO_TARGET_DIR detected; re-run with --allow-shared-target-dir to continue"
                );
            }
        }

        // todo: recursively find all git projects in the directory
        let dirs = [self.dir.clone()];

        let remove_unused_files = async {
            let stats = try_join_all(dirs.iter().map(async |dir| {
                self.remove_unused_files_of_cargo(dir.as_path(), &build_cmd)
                    .await
                    .with_context(|| {
                        format!("failed to clean up unused files in {}", dir.display())
                    })
            }))
            .await
            .context("failed to clean up unused files")?;

            let total = stats
                .into_iter()
                .fold(CleanupStats::default(), |mut acc, s| {
                    acc.merge_from(s);
                    acc
                });

            Ok::<_, anyhow::Error>(total)
        };

        let total_stats = remove_unused_files.await?;

        // Always show the per-profile size breakdown
        print_profile_breakdown(&total_stats);

        if total_stats.files == 0 {
            println!("✨ No unused artifacts found! Your target directory is already clean.");
            return Ok(());
        }

        // Show detailed summary of what will be removed
        print_detailed_summary(&total_stats);

        // Interactive confirmation if not in --yes mode
        let selection = if !self.yes {
            prompt_step_by_step(&total_stats)?
        } else if !self.dry_run {
            RemovalSelection {
                remove_files: true,
                remove_dirs: true,
            }
        } else {
            RemovalSelection::default()
        };

        if selection.any() {
            let removal_stats = self.actually_remove_files(&total_stats, &selection).await?;
            print_removal_summary(&removal_stats);
        } else {
            print_dry_run_summary(&total_stats);
        }

        Ok(())
    }
}
