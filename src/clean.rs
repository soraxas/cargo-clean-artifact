use std::{
    collections::HashMap,
    env,
    io::{self, IsTerminal, Write},
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

use crate::crate_deps::{crate_key, format_bytes, paint};
use crate::trace_parser::TraceParser;

/// Clean unused, old project files.
///
/// 1. This removes
///
///  - the unused files in `target` directory.
#[derive(Debug, Args)]
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
}

#[derive(Default)]
pub(super) struct CleanupStats {
    pub(super) files: usize,
    pub(super) bytes: u64,
    /// Total size of artifacts kept (in use)
    pub(super) used_bytes: u64,
    pub(super) per_crate: HashMap<String, CrateStat>,
    pub(super) per_profile: HashMap<String, ProfileStat>,
    pub(super) errors: HashMap<(String, String, String), anyhow::Error>,
    pub(super) files_to_remove: Vec<FileToRemove>,
}

#[derive(Default, Clone)]
pub(super) struct CrateStat {
    pub(super) files: usize,
    pub(super) bytes: u64,
}

#[derive(Default, Clone)]
pub(super) struct ProfileStat {
    pub(super) files: usize,
    pub(super) bytes: u64,
}

#[derive(Clone)]
pub(super) struct FileToRemove {
    pub(super) path: PathBuf,
    pub(super) size: u64,
    pub(super) profile: String,
}

impl CleanupStats {
    pub(super) fn merge_from(&mut self, other: CleanupStats) {
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
        }
        self.errors.extend(other.errors);
        self.files_to_remove.extend(other.files_to_remove);
    }
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
    ) -> Result<CleanupStats> {
        let cmd = self.custom_command.as_deref().expect("command required");

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

        // Compute in-use size from traced artifacts
        let mut used_bytes = 0u64;
        for artifact in &trace_result.used_artifacts {
            if let Ok(meta) = tokio::fs::metadata(artifact).await {
                used_bytes += meta.len();
            }
        }

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

        let mut stats = CleanupStats {
            used_bytes,
            ..Default::default()
        };
        let mut found_any_profile = false;

        for (deps_dir, display_profile) in &scan_dirs {
            if !deps_dir.exists() {
                log::debug!("Profile directory does not exist: {}", deps_dir.display());
                continue;
            }

            found_any_profile = true;

            let profile_stats = self
                .clean_with_trace_result(deps_dir, &trace_result.used_artifacts, display_profile)
                .await
                .context(format!("Failed to clean profile: {display_profile}"))?;

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
                continue;
            }

            // Keep any file whose crate name matches a current build output
            // (the root artifact is not in the trace since nothing depends on it)
            if protected_crate_names.contains(&crate_key(&path)) {
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
} // end impl CleanCommand (first block)

/// Extract the `crate_name-HASH` stem from any artifact file:
/// - `libfoo-HASH.rlib`              → `foo-HASH`
/// - `libfoo-HASH.rmeta`             → `foo-HASH`
/// - `foo-HASH.d`                    → `foo-HASH`
/// - `foo-HASH.foo.cgu.00.rcgu.dwo`  → `foo-HASH`
/// - `foo-HASH.foo.cgu.00.rcgu.o`    → `foo-HASH`
fn artifact_stem(path: &Path) -> Option<String> {
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

impl CleanCommand {
    async fn actually_remove_files(&self, stats: &CleanupStats) -> Result<CleanupStats> {
        let mut removal_stats = CleanupStats::default();

        let pb = ProgressBar::new(stats.files as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} files ({bytes_per_sec}) {msg}")
                .unwrap()
                .progress_chars("=>-"),
        );
        pb.set_message("Removing...");

        for file_info in &stats.files_to_remove {
            let crate_key = crate_key(&file_info.path);
            match fs::remove_file(&file_info.path).await {
                Ok(_) => {
                    removal_stats.files += 1;
                    removal_stats.bytes += file_info.size;
                    let entry = removal_stats
                        .per_crate
                        .entry(crate_key.clone())
                        .or_default();
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
                            crate_key.clone(),
                            file_info.profile.clone(),
                            file_info.path.display().to_string(),
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
        // Require --command; show a clap-style error with examples if missing
        if self.custom_command.is_none() {
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
                self.remove_unused_files_of_cargo(dir.as_path())
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

        if total_stats.files == 0 {
            println!("\n✨ No unused artifacts found! Your target directory is already clean.");
            return Ok(());
        }

        // Show detailed summary of what will be removed
        print_detailed_summary(&total_stats);

        // Interactive confirmation if not in --yes mode
        let should_remove = if !self.yes {
            prompt_confirmation(&total_stats)?
        } else {
            !self.dry_run || self.yes
        };

        if should_remove {
            let removal_stats = self.actually_remove_files(&total_stats).await?;
            print_removal_summary(&removal_stats);
        } else {
            print_dry_run_summary(&total_stats);
        }

        Ok(())
    }
}

fn prompt_confirmation(stats: &CleanupStats) -> Result<bool> {
    let color = io::stdout().is_terminal();
    let prompt_style = Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold();
    let size_style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();

    println!();
    println!(
        "{} Remove {} files ({})? [y/N]: ",
        paint(color, "❯", prompt_style),
        paint(color, stats.files.to_string(), size_style),
        paint(color, format_bytes(stats.bytes), size_style),
    );

    let mut input = String::new();
    io::stdout().flush()?;
    io::stdin().read_line(&mut input)?;

    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn print_detailed_summary(stats: &CleanupStats) {
    let color = io::stdout().is_terminal();
    let header_style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
    let profile_style = Style::new().fg_color(Some(AnsiColor::Magenta.into()));
    let file_style = Style::new().fg_color(Some(AnsiColor::Blue.into()));
    let size_style = Style::new().fg_color(Some(AnsiColor::Green.into()));
    let dim_style = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));

    println!();
    println!(
        "{} {}{}",
        paint(color, "📊 Summary:", header_style),
        paint(
            color,
            format!(
                "{} files ({}) can be removed",
                stats.files,
                format_bytes(stats.bytes)
            ),
            size_style
        ),
        if stats.used_bytes > 0 {
            paint(
                color,
                format!("  •  {} in use", format_bytes(stats.used_bytes)),
                dim_style,
            )
        } else {
            String::new()
        }
    );
    println!();

    // Show per-profile breakdown
    if !stats.per_profile.is_empty() {
        println!("{}", paint(color, "By profile:", header_style));
        for (profile, profile_stat) in &stats.per_profile {
            println!(
                "  {} {} files ({})",
                paint(color, format!("{profile}:"), profile_style),
                profile_stat.files,
                format_bytes(profile_stat.bytes)
            );
        }
        println!();
    }

    // Show top 10 files to be removed
    println!("{}", paint(color, "Top files to remove:", header_style));
    let mut files_sorted = stats.files_to_remove.clone();
    files_sorted.sort_by_key(|f| std::cmp::Reverse(f.size));

    for (i, file_info) in files_sorted.iter().take(10).enumerate() {
        let filename = file_info
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        println!(
            "  {}. {} {} ({})",
            i + 1,
            paint(color, &file_info.profile, profile_style),
            paint(color, filename, file_style),
            paint(color, format_bytes(file_info.size), size_style)
        );
    }

    if files_sorted.len() > 10 {
        println!(
            "  {} and {} more files...",
            paint(color, "...", file_style),
            files_sorted.len() - 10
        );
    }
}

fn print_removal_summary(stats: &CleanupStats) {
    let color = io::stdout().is_terminal();
    let success_style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    println!();
    println!(
        "{} {} files ({}) across {} crates",
        paint(color, "✓ Removed", success_style),
        paint(color, stats.files.to_string(), accent_style),
        paint(color, format_bytes(stats.bytes), accent_style),
        paint(color, stats.per_crate.len().to_string(), accent_style),
    );

    print_top_crates(stats, color);
    print_errors(stats, color);
}

fn print_dry_run_summary(stats: &CleanupStats) {
    let color = io::stdout().is_terminal();
    let dry_run_style = Style::new().fg_color(Some(AnsiColor::Yellow.into()));
    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    println!();
    println!(
        "{} would remove {} files ({}) across {} crates",
        paint(color, "Dry-run:", dry_run_style),
        paint(color, stats.files.to_string(), accent_style),
        paint(color, format_bytes(stats.bytes), accent_style),
        paint(color, stats.per_crate.len().to_string(), accent_style),
    );

    print_top_crates(stats, color);
}

fn print_top_crates(stats: &CleanupStats, color: bool) {
    let mut crates: Vec<_> = stats.per_crate.iter().collect();
    crates.sort_by_key(|(name, stat)| (std::cmp::Reverse(stat.bytes), name.to_string()));

    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    const MAX_CRATES: usize = 20;
    for (name, stat) in crates.iter().take(MAX_CRATES) {
        println!(
            "  - {}: {} files ({})",
            paint(color, name, accent_style),
            paint(color, stat.files.to_string(), accent_style),
            paint(color, format_bytes(stat.bytes), accent_style)
        );
    }

    if crates.len() > MAX_CRATES {
        println!(
            "  ... and {} more crates",
            paint(color, (crates.len() - MAX_CRATES).to_string(), accent_style)
        );
    }
}

fn print_errors(stats: &CleanupStats, color: bool) {
    if stats.errors.is_empty() {
        return;
    }

    let error_headline_style = Style::new().fg_color(Some(AnsiColor::Red.into())).bold();
    let error_accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));
    let error_crate_style = Style::new().fg_color(Some(AnsiColor::Yellow.into()));
    let error_flavor_style = Style::new().fg_color(Some(AnsiColor::Magenta.into()));
    let error_file_style = Style::new().fg_color(Some(AnsiColor::Blue.into()));

    println!(
        "\n{} {}",
        paint(color, "Errors:", error_headline_style),
        paint(
            color,
            format!("({} files)", stats.errors.len()),
            error_accent_style
        )
    );
    for ((crate_name, flavor, file), error) in stats.errors.iter() {
        println!(
            "  {} [{}]: {} -> {}",
            paint(color, crate_name, error_crate_style),
            paint(color, flavor, error_flavor_style),
            paint(color, file, error_file_style),
            paint(color, format!("{error}"), error_headline_style),
        );
    }
    println!();
}
