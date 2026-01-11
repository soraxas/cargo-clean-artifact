use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result};
use cargo_metadata::{CargoOpt, MetadataCommand};
use clap::{Args, ValueHint};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::crate_deps::{DepFile, crate_key, format_bytes, paint, read_deps_dir};

/// Analyze target artifacts to show which packages/features keep them alive.
#[derive(Debug, Args)]
pub(crate) struct AnalyseCommand {
    /// The directory to inspect.
    #[clap(
        value_hint = ValueHint::DirPath,
        default_value = ".",
        value_name = "DIR"
    )]
    dir: PathBuf,

    /// Target flavors to inspect, comma-separated (e.g. debug,release).
    #[clap(long, default_value = "debug,release", value_delimiter = ',')]
    flavors: Vec<String>,
}

#[derive(Clone)]
struct PackageInfo {
    name: String,
    features: Vec<String>,
}

#[derive(Debug, Clone)]
struct PackageUsage {
    name: String,
    features: Vec<String>,
}

#[derive(Debug)]
struct ArtifactUsage {
    crate_key: String,
    flavor: String,
    path: PathBuf,
    size: u64,
    used_by: Vec<PackageUsage>,
}

impl AnalyseCommand {
    pub async fn run(self) -> Result<()> {
        let metadata = MetadataCommand::new()
            .current_dir(&self.dir)
            .features(CargoOpt::AllFeatures)
            .exec()
            .context("failed to load cargo metadata")?;

        let target_dir = metadata.target_directory.as_std_path().to_path_buf();

        let feature_map: HashMap<String, Vec<String>> = metadata
            .resolve
            .as_ref()
            .map(|resolve| {
                resolve
                    .nodes
                    .iter()
                    .map(|node| (node.id.to_string(), node.features.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let packages: HashMap<PathBuf, PackageInfo> = metadata
            .packages
            .iter()
            .map(|pkg| {
                let features = feature_map
                    .get(&pkg.id.to_string())
                    .cloned()
                    .unwrap_or_default();
                let dir = pkg
                    .manifest_path
                    .parent()
                    .expect("manifest path has parent")
                    .as_std_path()
                    .to_path_buf();
                (
                    dir,
                    PackageInfo {
                        name: pkg.name.clone(),
                        features,
                    },
                )
            })
            .collect();

        let flavors = if self.flavors.is_empty() {
            vec!["debug".to_string(), "release".to_string()]
        } else {
            self.flavors.clone()
        };

        let mut usages = vec![];

        for flavor in flavors {
            let deps_dir = target_dir.join(&flavor).join("deps");
            if !deps_dir.exists() {
                continue;
            }

            let dep_files = read_deps_dir(&deps_dir)
                .await
                .with_context(|| format!("failed to read deps dir {}", deps_dir.display()))?;

            for dep in dep_files {
                let mut artifacts = analyze_dep_file(&dep, &packages, &target_dir, &flavor).await?;
                usages.append(&mut artifacts);
            }
        }

        print_analysis(&usages);

        Ok(())
    }
}

async fn analyze_dep_file(
    dep: &DepFile,
    packages: &HashMap<PathBuf, PackageInfo>,
    target_dir: &Path,
    flavor: &str,
) -> Result<Vec<ArtifactUsage>> {
    let mut used: HashMap<String, PackageUsage> = HashMap::new();

    for deps in dep.map.values() {
        for dep_path in deps {
            for dir in dep_path.ancestors() {
                if let Some(pkg) = packages.get(dir) {
                    used.entry(pkg.name.clone())
                        .or_insert_with(|| PackageUsage {
                            name: pkg.name.clone(),
                            features: pkg.features.clone(),
                        });
                    break;
                }
            }
        }
    }

    let mut artifacts = vec![];

    for (file, _) in dep.map.iter() {
        if !file.starts_with(target_dir) {
            continue;
        }

        if let Some(ext) = file.extension() {
            let ext = ext.to_string_lossy();
            if ext == "d" {
                continue;
            }
        }

        if !fs::try_exists(file).await? {
            continue;
        }

        let size = fs::metadata(file).await.map(|m| m.len()).unwrap_or(0);
        let crate_key = crate_key(file);

        artifacts.push(ArtifactUsage {
            crate_key,
            flavor: flavor.to_string(),
            path: file.clone(),
            size,
            used_by: used.values().cloned().collect(),
        });
    }

    Ok(artifacts)
}

fn print_analysis(usages: &[ArtifactUsage]) {
    let color = std::io::stdout().is_terminal();
    let headline_style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));
    let unused_style = Style::new().fg_color(Some(AnsiColor::Yellow.into()));

    println!(
        "{} {} artifacts",
        paint(color, "Analysis:", headline_style),
        paint(color, usages.len().to_string(), accent_style)
    );

    let mut by_crate: HashMap<String, Vec<&ArtifactUsage>> = HashMap::new();
    for usage in usages {
        by_crate
            .entry(usage.crate_key.clone())
            .or_default()
            .push(usage);
    }

    let mut entries: Vec<_> = by_crate.into_iter().collect();
    entries.sort_by(|a, b| {
        let a_size: u64 = a.1.iter().map(|u| u.size).sum();
        let b_size: u64 = b.1.iter().map(|u| u.size).sum();
        b_size.cmp(&a_size)
    });

    for (crate_key, mut group) in entries {
        let total_size: u64 = group.iter().map(|u| u.size).sum();
        group.sort_by(|a, b| b.size.cmp(&a.size));
        println!(
            "- {} ({} files, {})",
            paint(color, crate_key, accent_style),
            group.len(),
            paint(color, format_bytes(total_size), accent_style)
        );

        for usage in group {
            let used_by = if usage.used_by.is_empty() {
                paint(color, "unused", unused_style)
            } else {
                let mut names: Vec<String> = usage
                    .used_by
                    .iter()
                    .map(|p| {
                        if p.features.is_empty() {
                            p.name.clone()
                        } else {
                            format!("{}[{}]", p.name, p.features.join(","))
                        }
                    })
                    .collect();
                names.sort();
                names.join(", ")
            };

            println!(
                "  [{}] {} ({}) -> {}",
                paint(color, &usage.flavor, accent_style),
                usage.path.display(),
                paint(color, format_bytes(usage.size), accent_style),
                used_by
            );
        }
    }
}
