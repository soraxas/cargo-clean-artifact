//! Infer build selections from existing fingerprints without replaying a build.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use cargo_metadata::{MetadataCommand, Package};
use serde_json::Value;

/// One inferred command, ordered by the newest matching fingerprint timestamp.
pub(crate) struct DetectedCommand {
    pub(crate) command: String,
    pub(crate) summary: String,
    pub(crate) folder: String,
    pub(crate) folder_path: PathBuf,
    pub(crate) modified: SystemTime,
}

pub(crate) fn detect_commands(project_dir: &Path) -> Result<Vec<DetectedCommand>> {
    // No dependency resolution, network access, compilation, or build scripts.
    let metadata = MetadataCommand::new()
        .current_dir(project_dir)
        .no_deps()
        .other_options(vec!["--offline".to_owned()])
        .exec()
        .context("Failed to read workspace metadata for command detection")?;
    let project_dir = project_dir.canonicalize()?;
    let manifest = project_dir
        .ancestors()
        .map(|dir| dir.join("Cargo.toml"))
        .find(|path| path.is_file());
    let selected = metadata
        .packages
        .iter()
        .find(|package| manifest.as_deref() == Some(package.manifest_path.as_std_path()));
    let packages: Vec<_> = metadata
        .packages
        .iter()
        .filter(|p| {
            metadata.workspace_members.contains(&p.id)
                && selected.is_none_or(|selected| selected.id == p.id)
        })
        .collect();
    let target_dir = metadata.target_directory.as_std_path();
    let mut commands = HashMap::<String, DetectedCommand>::new();
    for (profile_dir, target) in profile_dirs(target_dir) {
        let Some(profile) = profile_dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        for entry in directories(&profile_dir.join(".fingerprint")) {
            let Some(name) = entry.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Split from the right: package names can themselves contain '-'.
            let Some((package_name, hash)) = name.rsplit_once('-') else {
                continue;
            };
            let Some(package) = packages.iter().find(|p| p.name == package_name) else {
                continue;
            };
            let Ok(files) = fs::read_dir(&entry) else {
                continue;
            };
            for file in files.flatten() {
                if !file.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let path = file.path();
                let Some(filename) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some((selection, source, target_name)) = target_selection(package, filename)
                else {
                    continue;
                };
                let candidate = (|| -> Result<(String, String)> {
                    let mut depfile = profile_dir
                        .join("deps")
                        .join(format!("{}-{hash}.d", target_name.replace('-', "_")));
                    if !depfile.exists() {
                        // Native dynamic libraries can omit the artifact hash.
                        // Do not use a newer, shared depfile to revive an older
                        // fingerprint (or one left over from a failed rebuild).
                        depfile = profile_dir
                            .join("deps")
                            .join(format!("{}.d", target_name.replace('-', "_")));
                        anyhow::ensure!(
                            file.metadata()?.modified()? >= fs::metadata(&depfile)?.modified()?,
                            "unhashed depfile is newer than this fingerprint"
                        );
                    }
                    anyhow::ensure!(
                        owns_source(&depfile, source, metadata.workspace_root.as_std_path())?,
                        "depfile does not reference the workspace target's source"
                    );
                    let record: Value = serde_json::from_slice(&fs::read(&path)?)?;
                    let package_spec = format!("{}@{}", package.name, package.version);
                    infer_command(
                        &record,
                        target.as_deref(),
                        profile,
                        (selected.is_none() || metadata.workspace_members.len() > 1)
                            .then_some(package_spec.as_str()),
                        &selection,
                        selected.is_none(),
                    )
                })();
                match candidate {
                    Ok((command, summary)) => {
                        let modified = file
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        commands
                            .entry(command.clone())
                            .and_modify(|old| old.modified = old.modified.max(modified))
                            .or_insert(DetectedCommand {
                                command,
                                summary,
                                modified,
                                folder_path: profile_dir.clone(),
                                folder: profile_dir
                                    .strip_prefix(target_dir)?
                                    .to_string_lossy()
                                    .into_owned(),
                            });
                    }
                    Err(error) => log::debug!("Skipping {}: {error}", path.display()),
                }
            }
        }
    }
    let mut commands: Vec<_> = commands.into_values().collect();
    commands.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.command.cmp(&b.command))
    });
    Ok(commands)
}

fn directories(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        // Do not follow symlinks into other caches.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

fn profile_dirs(target_dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    let mut profiles = Vec::new();
    for dir in directories(target_dir) {
        if dir.join(".fingerprint").is_dir() {
            profiles.push((dir, None));
        } else if !dir.join(".rustc_info.json").exists() {
            // A nested CARGO_TARGET_DIR is a separate cache, not a target triple.
            let Some(target) = dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            for profile in directories(&dir) {
                if profile.join(".fingerprint").is_dir() {
                    profiles.push((profile, Some(target.to_owned())));
                }
            }
        }
    }
    profiles
}

/// Match only workspace build targets, excluding dependencies, tests, examples,
/// and host-side build scripts. A package's single binary and library share the
/// default build selection; multiple binaries need an explicit selector.
fn target_selection<'a>(
    package: &'a Package,
    filename: &str,
) -> Option<(Vec<String>, &'a Path, &'a str)> {
    let (kind, name) = filename.strip_suffix(".json")?.split_once('-')?;
    if !matches!(kind, "bin" | "lib") {
        return None;
    }
    let target = package.targets.iter().find(|target| {
        let is_bin = target.kind.iter().any(|k| k == "bin");
        let is_lib = target.kind.iter().any(|k| {
            matches!(
                k.as_str(),
                "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
            )
        });
        ((kind == "bin" && is_bin) || (kind == "lib" && is_lib))
            && (target.name == name || target.name.replace('-', "_") == name)
    })?;
    let binaries = package
        .targets
        .iter()
        .filter(|t| t.kind.iter().any(|k| k == "bin"))
        .count();
    let selection = if binaries > 1 {
        if kind == "bin" {
            vec!["--bin".to_owned(), target.name.clone()]
        } else {
            vec!["--lib".to_owned()]
        }
    } else {
        Vec::new()
    };
    Some((selection, target.src_path.as_std_path(), &target.name))
}

/// Package names alone cannot distinguish workspace crates from dependencies of
/// another version. rustc's deps/*.d prerequisites identify the actual source;
/// relative paths are relative to the workspace root, even from member cwd's.
fn owns_source(depfile: &Path, source: &Path, workspace: &Path) -> Result<bool> {
    let contents = fs::read_to_string(depfile)?
        .replace("\\\r\n", "")
        .replace("\\\n", "");
    let rule = contents.lines().next().context("empty depfile")?;
    let (_, prerequisites) = rule.split_once(": ").context("invalid depfile rule")?;
    let source = source.canonicalize()?;
    // rustc lists the crate root first. Matching any imported module could
    // mistake a dependency that includes this package's source for the package.
    Ok(depfile_paths(prerequisites).first().is_some_and(|path| {
        workspace
            .join(path)
            .canonicalize()
            .is_ok_and(|p| p == source)
    }))
}

fn depfile_paths(text: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut token = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && chars
                .peek()
                .is_some_and(|c| c.is_whitespace() || matches!(c, '\\' | '#'))
        {
            token.push(chars.next().unwrap());
        } else if ch.is_whitespace() {
            if !token.is_empty() {
                paths.push(PathBuf::from(std::mem::take(&mut token)));
            }
        } else {
            token.push(ch);
        }
    }
    if !token.is_empty() {
        paths.push(PathBuf::from(token));
    }
    paths
}

fn infer_command(
    record: &Value,
    target: Option<&str>,
    profile: &str,
    package: Option<&str>,
    selection: &[String],
    show_package: bool,
) -> Result<(String, String)> {
    let compile_kind = record["compile_kind"]
        .as_u64()
        .context("missing compile_kind")?;
    anyhow::ensure!(
        (compile_kind == 0) == target.is_none(),
        "fingerprint target kind does not match directory layout"
    );
    let mut features: Vec<String> = serde_json::from_str(
        record["features"]
            .as_str()
            .context("missing feature list")?,
    )?;
    features.sort();
    features.dedup();
    let defaults = features.iter().any(|f| f == "default");
    features.retain(|f| f != "default");
    let rustflags: Vec<String> = serde_json::from_value(record["rustflags"].clone())?;
    let mut args = vec!["cargo".to_owned(), "build".to_owned()];
    if let Some(target) = target {
        args.extend(["--target".to_owned(), target.to_owned()]);
    }
    args.extend([
        "--profile".to_owned(),
        if profile == "debug" { "dev" } else { profile }.to_owned(),
    ]);
    if !defaults {
        args.push("--no-default-features".to_owned());
    }
    if !features.is_empty() {
        args.extend(["--features".to_owned(), features.join(",")]);
    }
    if let Some(package) = package {
        args.extend(["--package".to_owned(), package.to_owned()]);
    }
    args.extend_from_slice(selection);
    let command = args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let summary = build_summary(
        &features,
        defaults,
        &rustflags,
        package.filter(|_| show_package),
        selection,
    );
    let command = if rustflags.is_empty() {
        command
    } else {
        // Cargo's encoded form retains argument boundaries, including spaces in
        // --cfg values; ordinary RUSTFLAGS would split them again.
        format!(
            "CARGO_ENCODED_RUSTFLAGS={} {command}",
            shell_quote(&rustflags.join("\u{1f}"))
        )
    };
    Ok((command, summary))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./,:=+-@".contains(&b))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn build_summary(
    features: &[String],
    defaults: bool,
    rustflags: &[String],
    package: Option<&str>,
    selection: &[String],
) -> String {
    let mut shown_features: Vec<&str> = Vec::new();
    if defaults {
        shown_features.push("default");
    }
    shown_features.extend(features.iter().map(String::as_str));
    let mut parts = vec![format!(
        "features: {}",
        if shown_features.is_empty() {
            "none".to_owned()
        } else {
            shown_features.join(", ")
        }
    )];
    if let Some(package) = package {
        parts.push(format!("package: {package}"));
    }
    if !selection.is_empty() {
        parts.push(selection.join(" "));
    }
    if !rustflags.is_empty() {
        parts.push(format!(
            "rustflags: {}",
            rustflags
                .iter()
                .map(|flag| shell_quote(flag))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    #[test]
    fn summaries_expose_feature_differences_without_repeating_target_or_profile() {
        let newest = build_summary(
            &["brp".to_owned(), "webgpu".to_owned()],
            false,
            &[],
            None,
            &[],
        );
        let older = build_summary(&["webgpu".to_owned()], false, &[], None, &[]);
        assert_eq!(newest, "features: brp, webgpu");
        assert_eq!(older, "features: webgpu");
        assert_eq!(build_summary(&[], false, &[], None, &[]), "features: none");
    }

    #[test]
    fn summaries_do_not_hide_defaults_flags_package_or_binary_differences() {
        let features = vec!["webgpu".to_owned()];
        assert_eq!(
            build_summary(&features, true, &[], None, &[]),
            "features: default, webgpu"
        );
        let summary = build_summary(
            &features,
            false,
            &["--cfg=debug_tools".to_owned()],
            Some("demo@0.2.0"),
            &["--bin".to_owned(), "viewer".to_owned()],
        );
        assert!(summary.contains("--cfg=debug_tools"));
        assert!(summary.contains("demo@0.2.0"));
        assert!(summary.contains("viewer"));
        assert_ne!(summary, build_summary(&features, false, &[], None, &[]));
    }
}
