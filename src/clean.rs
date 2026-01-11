use std::{
    collections::HashMap,
    io::IsTerminal,
    path::{Path, PathBuf},
};

use anstyle::{AnsiColor, Reset, Style};
use anyhow::{Context, Result};
use cargo_metadata::{CargoOpt, MetadataCommand};
use clap::ArgAction;
use clap::{Args, ValueHint};
use futures::{future::try_join_all, try_join};
use tokio::fs;

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
}

#[derive(Default, Clone)]
pub(super) struct CleanupStats {
    pub(super) files: usize,
    pub(super) bytes: u64,
    pub(super) per_crate: HashMap<String, CrateStat>,
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
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GiB", b / GB)
    } else if b >= MB {
        format!("{:.2} MiB", b / MB)
    } else if b >= KB {
        format!("{:.2} KiB", b / KB)
    } else {
        format!("{} B", bytes)
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
        let mut stats = CleanupStats::default();

        for dep in dep_files {
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
                continue;
            }

            for (file, _) in dep.map.iter() {
                if file.ancestors().all(|dir| dir != target_dir) {
                    continue;
                }

                if let Some(ext) = file.extension() {
                    if ext == "rlib" || ext == "rmeta" {
                        // We only delete rlib and rmeta
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }

                let size = fs::metadata(file).await.map(|m| m.len()).unwrap_or(0);
                let crate_key = crate_key(file);

                if !self.is_dry_run() {
                    let _ = fs::remove_file(file).await;
                }

                stats.files += 1;
                stats.bytes += size;

                let entry = stats.per_crate.entry(crate_key).or_default();
                entry.files += 1;
                entry.bytes += size;
            }
        }

        Ok(stats)
    }

    fn is_dry_run(&self) -> bool {
        !self.yes || self.dry_run
    }

    pub async fn run(self) -> Result<()> {
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

        print_summary(self.is_dry_run(), &total_stats);

        Ok(())
    }
}

fn crate_key(path: &Path) -> String {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // Strip common prefixes like `lib`.
    let name = name.strip_prefix("lib").unwrap_or(name);

    // Drop trailing build hash if present: libfoo-<hash>.rlib
    name.split('-').next().unwrap_or(name).to_string()
}

/// .d file
#[derive(Debug)]
struct DepFile {
    map: HashMap<PathBuf, Vec<PathBuf>>,
}

async fn read_deps_dir(dir: &Path) -> Result<Vec<DepFile>> {
    let mut entries = fs::read_dir(dir).await?;
    let mut files = vec![];

    while let Some(e) = entries.next_entry().await? {
        if e.path().extension().map_or(false, |ext| ext == "d") {
            let content = fs::read_to_string(e.path()).await?;
            let file = parse_dep_file(&content)?;
            files.push(file);
        }
    }

    Ok(files)
}

fn parse_dep_file(s: &str) -> Result<DepFile> {
    let entries = s
        .lines()
        .map(|s| s.trim())
        .filter(|&s| !s.is_empty())
        .map(|line| line.split_once(':').unwrap())
        .map(|(k, v)| {
            (
                PathBuf::from(k),
                v.split_whitespace().map(PathBuf::from).collect(),
            )
        })
        .collect();

    Ok(DepFile { map: entries })
}

fn paint(enabled: bool, text: impl AsRef<str>, style: Style) -> String {
    if !enabled {
        return text.as_ref().to_string();
    }
    format!("{style}{}{}", text.as_ref(), Reset)
}

fn print_summary(dry_run: bool, total_stats: &CleanupStats) {
    let mut crates: Vec<_> = total_stats.per_crate.iter().collect();
    crates.sort_by_key(|(_, stat)| std::cmp::Reverse(stat.bytes));

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

    for (name, stat) in crates.iter().take(20) {
        println!(
            "- {}: {} files ({})",
            paint(color, name.to_string(), accent_style),
            paint(color, stat.files.to_string(), accent_style),
            paint(color, format_bytes(stat.bytes), accent_style)
        );
    }

    if crates.len() > 20 {
        println!(
            "... and {} more crates",
            paint(color, (crates.len() - 20).to_string(), accent_style)
        );
    }
}
