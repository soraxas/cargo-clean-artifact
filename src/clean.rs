use std::{
    collections::HashMap,
    env,
    io::IsTerminal,
    path::{Path, PathBuf},
};

use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result};
use cargo_metadata::{CargoOpt, MetadataCommand};
use clap::ArgAction;
use clap::{Args, ValueHint};
use futures::{future::try_join_all, try_join};
use tokio::fs;

use crate::crate_deps::{DepFile, crate_key, format_bytes, paint, read_deps_dir};
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
}

#[derive(Default)]
pub(super) struct CleanupStats {
    pub(super) files: usize,
    pub(super) bytes: u64,
    pub(super) per_crate: HashMap<String, CrateStat>,
    pub(super) errors: HashMap<(String, String, String), anyhow::Error>,
}

#[derive(Default, Clone)]
pub(super) struct CrateStat {
    pub(super) files: usize,
    pub(super) bytes: u64,
}

impl CleanupStats {
    pub(super) fn merge_from(&mut self, other: CleanupStats) {
        self.files += other.files;
        self.bytes += other.bytes;
        for (name, stat) in other.per_crate {
            let entry = self.per_crate.entry(name).or_default();
            entry.files += stat.files;
            entry.bytes += stat.bytes;
        }
        self.errors.extend(other.errors);
    }
}

impl CleanCommand {
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
        // Not a cargo project?
        // TODO: Log
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(_) => return Ok(CleanupStats::default()),
        };

        // Calculate current dependencies

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

        let target_dir = metadata.target_directory.as_std_path().to_path_buf();

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

        let dep_files = read_deps_dir(&base_dir.join("deps"))
            .await
            .context(format!(
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

        let total_stats =
            stats
                .drain(..)
                .filter_map(|s| s)
                .fold(CleanupStats::default(), |mut acc, s| {
                    acc.merge_from(s);
                    acc
                });

        Ok(total_stats)
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
                    format!("CARGO_TARGET_DIR is set to {}", target_dir),
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
        let dirs = vec![self.dir.clone()];

        let remove_unused_files = async {
            let stats = try_join_all(dirs.iter().map(async |dir| {
                self.remove_unused_files_of_cargo(&dir.as_path())
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

        print_summary(!self.yes, &total_stats);

        Ok(())
    }
}

fn print_summary(dry_run: bool, total_stats: &CleanupStats) {
    let mut crates: Vec<_> = total_stats.per_crate.iter().collect();
    crates.sort_by_key(|(name, stat)| (std::cmp::Reverse(stat.bytes), name.to_string()));

    let color = std::io::stdout().is_terminal();
    let headline_style = Style::new().fg_color(Some(if dry_run {
        AnsiColor::Yellow.into()
    } else {
        AnsiColor::Green.into()
    }));
    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    let headline = if dry_run {
        format!(
            "{} would remove {} files ({}) across {} crates",
            paint(color, "Dry-run:", headline_style),
            paint(color, total_stats.files.to_string(), accent_style),
            paint(color, format_bytes(total_stats.bytes), accent_style),
            paint(color, crates.len().to_string(), accent_style),
        )
    } else {
        format!(
            "{} {} files ({}) across {} crates",
            paint(color, "Removed", headline_style),
            paint(color, total_stats.files.to_string(), accent_style),
            paint(color, format_bytes(total_stats.bytes), accent_style),
            paint(color, crates.len().to_string(), accent_style),
        )
    };

    println!("{headline}");
    const MAX_CRATES: usize = 20;

    for (name, stat) in crates.iter().take(MAX_CRATES) {
        println!(
            "- {}: {} files ({})",
            paint(color, name.to_string(), accent_style),
            paint(color, stat.files.to_string(), accent_style),
            paint(color, format_bytes(stat.bytes), accent_style)
        );
    }

    if crates.len() > MAX_CRATES {
        println!(
            "... and {} more crates",
            paint(color, (crates.len() - 20).to_string(), accent_style)
        );
    }

    if !total_stats.errors.is_empty() {
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
                format!("({} crates)", total_stats.errors.len()),
                error_accent_style
            )
        );
        for ((crate_name, flavor, file), error) in total_stats.errors.iter() {
            println!(
                "  {} [{}]: {} -> {}",
                paint(color, crate_name, error_crate_style),
                paint(color, flavor, error_flavor_style),
                paint(color, file, error_file_style),
                paint(color, format!("{}", error), error_headline_style),
            );
        }
        println!(); // Add a trailing blank for separation
    }
}
