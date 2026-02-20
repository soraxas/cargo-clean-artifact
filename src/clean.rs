use std::{
    collections::HashMap,
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result};
use cargo_metadata::{CargoOpt, MetadataCommand};
use clap::ArgAction;
use clap::{Args, ValueHint};
use dialoguer::{MultiSelect, theme::ColorfulTheme};
use futures::{future::try_join_all, try_join};
use tokio::fs;

use indicatif::{ProgressBar, ProgressStyle};

use crate::crate_deps::{DepFile, crate_key, format_bytes, paint, read_deps_dir};
use crate::trace_parser::{TraceMode, TraceParser};

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

    /// Use cargo check trace mode (light, faster but may miss some artifacts).
    #[clap(long = "check-mode", conflicts_with = "build_mode")]
    check_mode: bool,

    /// Use cargo build trace mode (full, slower but more complete).
    #[clap(long = "build-mode", conflicts_with = "check_mode")]
    build_mode: bool,

    /// Custom build command to trace (e.g. "cargo build --release --target x86_64-unknown-linux-gnu").
    /// If specified, this overrides --check-mode and --build-mode.
    #[clap(long = "command", value_name = "COMMAND", conflicts_with_all = &["check_mode", "build_mode"])]
    custom_command: Option<String>,

    /// Profiles to check (default: debug). Can be specified multiple times.
    #[clap(long = "profile", value_name = "PROFILE")]
    profiles: Vec<String>,

    /// Build with all features enabled (thorough but may be slower).
    #[clap(long = "all-features")]
    all_features: bool,

    /// Build with no default features.
    #[clap(long = "no-default-features")]
    no_default_features: bool,

    /// Comma-separated list of features to activate.
    #[clap(long = "features", value_name = "FEATURES")]
    features: Option<String>,

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

    /// Detect the profile from a custom cargo command
    fn detect_profile_from_command(command: &str) -> String {
        let parts: Vec<&str> = command.split_whitespace().collect();

        // Look for --profile <name> or --release
        for i in 0..parts.len() {
            if parts[i] == "--release" {
                return "release".to_string();
            }
            if parts[i] == "--profile" && i + 1 < parts.len() {
                return parts[i + 1].to_string();
            }
        }

        // Default to debug
        "debug".to_string()
    }

    /// Clean up `target` of cargo.
    ///
    /// We only remove build outputs for outdated dependencies.
    pub(super) async fn remove_unused_files_of_cargo(
        &self,
        git_dir: &Path,
    ) -> Result<CleanupStats> {
        let metadata = MetadataCommand::new()
            .current_dir(git_dir)
            .features(CargoOpt::AllFeatures)
            .exec();
        
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

        // Check if target directory exists
        if !target_dir.exists() {
            eprintln!(
                "⚠️  Warning: Target directory does not exist: {}",
                target_dir.display()
            );
            eprintln!("   Run `cargo build` first to generate build artifacts.");
            return Ok(CleanupStats::default());
        }

        // Get project name and available features for feature detection
        let project_name = metadata
            .workspace_root
            .as_std_path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        // Get list of available features from the root package
        let available_features: Vec<String> = metadata
            .packages
            .iter()
            .find(|pkg| pkg.name == project_name)
            .map(|pkg| pkg.features.keys().cloned().collect())
            .unwrap_or_default();

        // Use trace mode if requested
        if self.check_mode || self.build_mode || self.custom_command.is_some() {
            return self
                .remove_unused_files_with_trace(
                    git_dir,
                    &target_dir,
                    project_name,
                    &available_features,
                )
                .await;
        }

        // Otherwise use the legacy .d file method
        let used_package_dirs = metadata
            .packages
            .iter()
            .map(|pkg| {
                pkg.manifest_path
                    .parent()
                    .unwrap()
                    .as_std_path()
                    .to_path_buf()
            })
            .collect::<Vec<_>>();

        let (debug, release) = try_join!(
            async {
                self.clean_one_target(&used_package_dirs, &target_dir, "debug")
                    .await
                    .context("failed to clean debug target")
            },
            async {
                self.clean_one_target(&used_package_dirs, &target_dir, "release")
                    .await
                    .context("failed to clean release target")
            },
        )?;

        let mut total = CleanupStats::default();
        total.merge_from(debug);
        total.merge_from(release);

        Ok(total)
    }

    /// Remove unused files using cargo trace mode
    async fn remove_unused_files_with_trace(
        &self,
        project_dir: &Path,
        target_dir: &Path,
        project_name: &str,
        available_features: &[String],
    ) -> Result<CleanupStats> {
        let mode = if self.check_mode {
            TraceMode::Check
        } else {
            TraceMode::Build
        };

        // Detect profiles from custom command or use specified profiles
        let profiles = if let Some(ref cmd) = self.custom_command {
            // Parse profile from custom command
            vec![Self::detect_profile_from_command(cmd)]
        } else if self.profiles.is_empty() {
            vec!["debug".to_string()]
        } else {
            self.profiles.clone()
        };

        // Build feature configuration - auto-detect if no flags specified and no custom command
        let feature_config = if self.custom_command.is_some() {
            // Custom command - don't add feature flags automatically
            crate::trace_parser::FeatureConfig::default()
        } else if self.all_features || self.no_default_features || self.features.is_some() {
            // User explicitly specified features
            crate::trace_parser::FeatureConfig {
                all_features: self.all_features,
                no_default_features: self.no_default_features,
                features: self.features.clone(),
            }
        } else {
            // Auto-detect from fingerprints, validating against available features
            let profile_name = if profiles[0] == "dev" {
                "debug"
            } else {
                &profiles[0]
            };
            crate::trace_parser::FeatureConfig::auto_detect_from_fingerprints(
                target_dir,
                profile_name,
                project_name,
                available_features,
            )
            .await
            .unwrap_or_default()
        };

        // Record time before the trace so we can exclude files created/modified during the build
        // (e.g. the final output artifact — not a dep so not in the trace, but freshly produced).
        let trace_start = SystemTime::now();

        let parser = TraceParser::new(target_dir.to_path_buf());
        let trace_result = parser
            .trace_profiles(
                project_dir,
                mode,
                &profiles,
                &feature_config,
                self.custom_command.as_deref(),
            )
            .await
            .context("Failed to trace cargo build")?;

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
            if let Some(parent) = artifact.parent() {
                if parent.file_name().map_or(false, |n| n == "deps")
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
        }

        // Also add explicitly requested profile dirs (in case nothing was traced there yet).
        // Only do this for built-in modes (check/build), not custom commands — a custom command
        // may build for a different target entirely (e.g. wasm) and should only clean what it
        // actually traced.
        if self.custom_command.is_none() {
            for profile in &profiles {
                let profile_name = if profile == "dev" { "debug" } else { profile };
                let deps_dir = target_dir.join(profile_name).join("deps");
                if deps_dir.exists() && !scan_dirs.iter().any(|(d, _)| d == &deps_dir) {
                    scan_dirs.push((deps_dir, profile_name.to_string()));
                }
            }
        }

        log::debug!("Scanning {} deps directories", scan_dirs.len());
        for (dir, name) in &scan_dirs {
            log::debug!("  {} ({})", name, dir.display());
        }

        // Now scan target directory and find artifacts not in the trace
        let mut stats = CleanupStats { used_bytes, ..Default::default() };
        let mut found_any_profile = false;

        for (deps_dir, display_profile) in &scan_dirs {
            if !deps_dir.exists() {
                log::debug!("Profile directory does not exist: {}", deps_dir.display());
                continue;
            }

            found_any_profile = true;

            let profile_stats = self
                .clean_with_trace_result(deps_dir, &trace_result.used_artifacts, display_profile, trace_start)
                .await
                .context(format!("Failed to clean profile: {display_profile}"))?;

            stats.merge_from(profile_stats);
        }

        if !found_any_profile {
            eprintln!("⚠️  Warning: No profile directories found in target directory");
            eprintln!("   Checked profiles: {}", profiles.join(", "));
            eprintln!("   Target directory: {}", target_dir.display());
            eprintln!("   Run `cargo build` first to generate build artifacts.");
        }

        Ok(stats)
    }

    /// Clean artifacts in a deps directory based on trace results.
    /// Uses stem-based grouping: all files sharing a `crate-HASH` stem with a
    /// used `.rlib`/`.rmeta` are kept. This catches `.dwo`, `.o`, `.d`, etc.
    async fn clean_with_trace_result(
        &self,
        deps_dir: &Path,
        used_artifacts: &std::collections::HashSet<PathBuf>,
        profile: &str,
        trace_start: SystemTime,
    ) -> Result<CleanupStats> {
        // Build the set of used stems from artifacts that live in this deps dir
        let mut used_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
        for artifact in used_artifacts {
            if artifact.parent() == Some(deps_dir) {
                if let Some(stem) = artifact_stem(artifact) {
                    used_stems.insert(stem);
                }
            }
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

            // Any file sharing a stem with a used artifact is kept
            if used_stems.contains(&stem) {
                continue;
            }

            // Skip files that were created/modified during (or after) the trace run —
            // they are the freshly-produced output artifacts, not stale leftovers.
            let meta = fs::metadata(&path).await;
            if let Ok(ref m) = meta {
                if m.modified().map_or(false, |mtime| mtime >= trace_start) {
                    continue;
                }
            }

            // Unused – mark for removal
            let size = meta.map(|m| m.len()).unwrap_or(0);
            let crate_key = crate_key(&path);

            stats.files_to_remove.push(FileToRemove {
                path: path.clone(),
                size,
                profile: profile.to_string(),
            });
            stats.files += 1;
            stats.bytes += size;
            let entry = stats.per_crate.entry(crate_key).or_default();
            entry.files += 1;
            entry.bytes += size;
            let profile_entry = stats.per_profile.entry(profile.to_string()).or_default();
            profile_entry.files += 1;
            profile_entry.bytes += size;
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

    async fn process_dep_file(
        &self,
        dep: &DepFile,
        used_package_dirs: &[PathBuf],
        target_dir: &Path,
        dry_run: bool,
        flavor: &str,
    ) -> Result<Option<CleanupStats>> {
        // Skip dep files that touch used package dirs.
        if dep.map.values().any(|deps| {
            deps.iter().any(|dep| {
                dep.ancestors().any(|dir| {
                    used_package_dirs
                        .iter()
                        .any(|used_package_dir| used_package_dir == dir)
                })
            })
        }) {
            return Ok(None);
        }

        let mut stats = CleanupStats::default();

        for (file, _) in dep.map.iter() {
            if file.ancestors().all(|dir| dir != target_dir) {
                return Ok(None);
            }

            if let Some(ext) = file.extension() {
                if ext == "rlib" || ext == "rmeta" {
                    // We only delete rlib and rmeta
                } else {
                    return Ok(None);
                }
            } else {
                return Ok(None);
            }

            if !fs::try_exists(file).await? {
                return Ok(None);
            }

            let size = fs::metadata(file).await.map(|m| m.len()).unwrap_or(0);
            let crate_key = crate_key(file);

            fn update_stats(stats: &mut CleanupStats, crate_key: &str, size: u64) {
                stats.files += 1;
                stats.bytes += size;
                let entry = stats.per_crate.entry(crate_key.to_string()).or_default();
                entry.files += 1;
                entry.bytes += size;
            }

            if dry_run {
                println!("Would remove {}", file.display());
                update_stats(&mut stats, &crate_key, size);
            } else if self.yes {
                match fs::remove_file(file).await {
                    Ok(_) => {
                        update_stats(&mut stats, &crate_key, size);
                    }
                    Err(e) => {
                        stats.errors.insert(
                            (
                                crate_key.clone(),
                                flavor.to_string(),
                                file.display().to_string(),
                            ),
                            e.into(),
                        );
                    }
                };
            } else {
                update_stats(&mut stats, &crate_key, size);
            }
        }
        Ok(Some(stats))
    }

    async fn clean_one_target(
        &self,
        used_package_dirs: &[PathBuf],
        target_dir: &Path,
        flavor: &str,
    ) -> Result<CleanupStats> {
        let base_dir = target_dir.join(flavor);

        if !base_dir.exists() {
            return Ok(CleanupStats::default());
        }

        let deps_dir = base_dir.join("deps");
        if !deps_dir.exists() {
            log::debug!("Deps directory does not exist: {}", deps_dir.display());
            return Ok(CleanupStats::default());
        }

        let dep_files = read_deps_dir(&deps_dir).await.context(format!(
            "failed to read cargo deps at {}",
            base_dir.display()
        ))?;

        let dry_run = self.dry_run;

        let mut stats = try_join_all(dep_files.iter().map(async |dep| {
            self.process_dep_file(dep, used_package_dirs, target_dir, dry_run, flavor)
                .await
                .context("failed to process dep file")
        }))
        .await
        .context("failed to process dep files")?;

        let total_stats = stats
            .drain(..)
            .flatten()
            .fold(CleanupStats::default(), |mut acc, s| {
                acc.merge_from(s);
                acc
            });

        Ok(total_stats)
    }

    /// Detect available profiles in the target directory
    async fn detect_profiles(target_dir: &Path) -> Result<Vec<String>> {
        let mut profiles = Vec::new();

        let mut entries = fs::read_dir(target_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Check if it has a deps subdirectory (indicating it's a profile)
                    let deps_dir = path.join("deps");
                    if deps_dir.exists() && !name.starts_with('.') {
                        profiles.push(name.to_string());
                    }
                }
            }
        }

        profiles.sort();
        Ok(profiles)
    }

    /// Show interactive profile selector if no profiles specified
    fn select_profiles_interactive(available: Vec<String>) -> Result<Vec<String>> {
        if available.is_empty() {
            anyhow::bail!("No profiles found in target directory");
        }

        println!("\n📊 Available profiles in target/:");

        let selections = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select profiles to clean (Space to select, Enter to confirm)")
            .items(&available)
            .defaults(&vec![true; available.len()]) // All selected by default
            .interact()?;

        if selections.is_empty() {
            anyhow::bail!("No profiles selected");
        }

        Ok(selections
            .into_iter()
            .map(|i| available[i].clone())
            .collect())
    }

    pub async fn run(self) -> Result<()> {
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

        // Interactive confirmation if using trace mode and not in --yes mode
        let should_remove =
            if (self.check_mode || self.build_mode || self.custom_command.is_some()) && !self.yes {
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
