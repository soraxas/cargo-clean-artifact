use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Mode for tracing cargo builds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceMode {
    /// Use `cargo check` - faster, only traces .rmeta files
    Check,
    /// Use `cargo build` - complete, traces .rlib and .rmeta files
    Build,
}

/// Feature configuration for cargo builds
#[derive(Debug, Clone, Default)]
pub struct FeatureConfig {
    /// Use all features
    pub all_features: bool,
    /// No default features
    pub no_default_features: bool,
    /// Specific features to enable
    pub features: Option<String>,
}

impl FeatureConfig {
    /// Auto-detect features from fingerprint files in target directory
    pub async fn auto_detect_from_fingerprints(
        target_dir: &Path,
        profile: &str,
        project_name: &str,
        available_features: &[String],
    ) -> Result<Self> {
        use serde_json::Value;
        use std::collections::HashSet;
        use tokio::fs;

        let fingerprint_dir = target_dir.join(profile).join(".fingerprint");

        if !fingerprint_dir.exists() {
            log::debug!("No fingerprint directory found, using defaults");
            return Ok(Self::default());
        }

        let mut detected_features: HashSet<String> = HashSet::new();

        // Normalize project name (replace - with _)
        let normalized_project_name = project_name.replace('-', "_");

        // Read all fingerprint files
        let mut entries = fs::read_dir(&fingerprint_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Only look at fingerprints for the project itself
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !dir_name.starts_with(&normalized_project_name)
                && !dir_name.starts_with(project_name)
            {
                continue;
            }

            // Look for JSON files in the project's fingerprint directory
            let mut json_entries = fs::read_dir(&path).await?;

            while let Some(json_entry) = json_entries.next_entry().await? {
                let json_path = json_entry.path();

                if json_path.extension().and_then(|s| s.to_str()) == Some("json") {
                    // Try to parse the JSON
                    if let Ok(content) = fs::read_to_string(&json_path).await {
                        // Split by newlines and parse each line as JSON
                        for line in content.lines() {
                            if let Ok(json) = serde_json::from_str::<Value>(line) {
                                // Extract features array
                                if let Some(features_str) =
                                    json.get("features").and_then(|v| v.as_str())
                                {
                                    // Parse the features string which looks like: "[\"default\", \"rayon\"]"
                                    if let Ok(features_array) =
                                        serde_json::from_str::<Vec<String>>(features_str)
                                    {
                                        detected_features.extend(features_array);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if detected_features.is_empty() {
            log::debug!("No features found in fingerprints for {project_name}, using defaults");
            return Ok(Self::default());
        }

        // Remove "default" from the list as it's implicit
        detected_features.remove("default");

        if detected_features.is_empty() {
            // Only "default" feature was used
            return Ok(Self::default());
        }

        // Create a set of available features for fast lookup
        let available_set: HashSet<String> = available_features.iter().cloned().collect();

        // Filter detected features to only include ones that currently exist
        let valid_features: Vec<String> = detected_features
            .iter()
            .filter(|f| available_set.contains(*f))
            .cloned()
            .collect();

        let invalid_features: Vec<String> = detected_features
            .iter()
            .filter(|f| !available_set.contains(*f))
            .cloned()
            .collect();

        // Show warning if some features are no longer available
        if !invalid_features.is_empty() {
            let mut sorted_invalid = invalid_features;
            sorted_invalid.sort();
            println!(
                "⚠️  Ignoring outdated features: {}",
                sorted_invalid.join(", ")
            );
        }

        if valid_features.is_empty() {
            // All detected features are outdated, use defaults
            Ok(Self::default())
        } else {
            let mut sorted_features = valid_features;
            sorted_features.sort();

            println!("🔎 Auto-detected features: {}", sorted_features.join(", "));

            Ok(Self {
                all_features: false,
                no_default_features: false,
                features: Some(sorted_features.join(",")),
            })
        }
    }

    pub fn is_default(&self) -> bool {
        !self.all_features && !self.no_default_features && self.features.is_none()
    }
}

/// Result of tracing a cargo build/check
#[derive(Debug, Default)]
pub struct TraceResult {
    /// Set of artifact paths that were referenced during the build
    pub used_artifacts: HashSet<PathBuf>,
}

/// Parser for cargo build trace output
pub struct TraceParser {
    target_dir: PathBuf,
}

impl TraceParser {
    pub fn new(target_dir: PathBuf) -> Self {
        Self { target_dir }
    }

    /// Run cargo with trace logging and collect used artifacts
    pub async fn trace(
        &self,
        project_dir: &Path,
        mode: TraceMode,
        profile: Option<&str>,
        feature_config: &FeatureConfig,
        custom_command: Option<&str>,
    ) -> Result<TraceResult> {
        let feature_desc = if feature_config.all_features {
            " with all features".to_string()
        } else if let Some(ref features) = feature_config.features {
            format!(" with features: {features}")
        } else if feature_config.no_default_features {
            " with no default features".to_string()
        } else {
            "".to_string()
        };

        if let Some(cmd_str) = custom_command {
            println!("🔍 Tracing with custom command: {cmd_str}...");
        } else {
            println!("🔍 Tracing dependencies using cargo {mode:?}{feature_desc}...");
        }

        let mut cmd = if let Some(cmd_str) = custom_command {
            // Parse custom command
            let parts: Vec<&str> = cmd_str.split_whitespace().collect();
            if parts.is_empty() {
                anyhow::bail!("Custom command is empty");
            }

            let mut command = Command::new(parts[0]);
            if parts.len() > 1 {
                command.args(&parts[1..]);
            }
            command
        } else {
            // Build standard cargo command
            let mut command = Command::new("cargo");

            // Set the command based on mode
            match mode {
                TraceMode::Check => {
                    command.arg("check");
                }
                TraceMode::Build => {
                    command.arg("build");
                }
            }

            // Add feature flags
            if feature_config.all_features {
                command.arg("--all-features");
            }
            if feature_config.no_default_features {
                command.arg("--no-default-features");
            }
            if let Some(ref features) = feature_config.features {
                command.arg("--features").arg(features);
            }

            // Add profile if specified
            if let Some(prof) = profile {
                if prof != "dev" && prof != "debug" {
                    command.arg("--profile").arg(prof);
                } else if prof == "release" {
                    command.arg("--release");
                }
            }

            command
        };

        cmd.current_dir(project_dir);

        // Set trace logging
        cmd.env("CARGO_LOG", "cargo::core::compiler::fingerprint=trace")
            .env("CARGO_TERM_COLOR", "always");

        // Capture both stdout and stderr
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn cargo command")?;

        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;

        let mut result = TraceResult::default();

        // Get readers for both stdout and stderr
        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        // Create progress spinner
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        spinner.set_message("Tracing dependencies... (0 artifacts found)");
        spinner.enable_steady_tick(std::time::Duration::from_millis(100));

        // Read both streams concurrently
        let mut cur_status_line = None;
        loop {
            tokio::select! {
                // Discard stdout (cargo's compilation progress)
                stdout_line = stdout_reader.next_line() => {
                    match stdout_line? {
                        Some(line) => {
                            // Silently consume stdout
                            spinner.set_message(line);
                        }
                        None => {
                            // stdout closed
                        }
                    }
                }
                // Parse stderr for trace logs
                stderr_line = stderr_reader.next_line() => {
                    match stderr_line? {
                        Some(line) => {
                            if let Some(path) = self.extract_artifact_path(&line) {
                                // has artifact path
                                result.used_artifacts.insert(path);
                            } else if !line.contains("cargo::core::compiler::fingerprint:") && !line.trim().is_empty(){
                                // this not a log line. store for display
                                cur_status_line = Some(line);
                            }
                            spinner.set_message(format!(
                                "Tracing dependencies... ({} artifacts found)\n{}",
                                result.used_artifacts.len(),
                                &cur_status_line.as_ref().unwrap_or(&"".to_string())
                            ));
                        }
                        None => {
                            // stderr closed, we're done
                            break;
                        }
                    }
                }
            }
        }

        // Wait for command to complete
        let status = child.wait().await?;

        spinner.finish_with_message(format!(
            "✅ Trace complete: found {} artifacts in use",
            result.used_artifacts.len()
        ));
        println!(); // Blank line after spinner

        if !status.success() {
            anyhow::bail!("Cargo command failed with status: {status}");
        }

        Ok(result)
    }

    /// Extract artifact path from a trace log line
    fn extract_artifact_path(&self, line: &str) -> Option<PathBuf> {
        // Look for patterns like:
        // max output mtime for "..." is "/path/to/target/debug/deps/libfoo.rlib"
        // max dep mtime for "..." is "/path/to/target/debug/deps/libbar.rmeta"

        if !line.contains("mtime") {
            return None;
        }

        // Find the last quoted path in the line
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() < 2 {
            return None;
        }

        // The path is typically the last quoted string
        for part in parts.iter().rev() {
            let path = PathBuf::from(part);

            // Check if it's a relevant artifact file
            if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy();
                if ext_str == "rlib"
                    || ext_str == "rmeta"
                    || ext_str == "so"
                    || ext_str == "dylib"
                    || ext_str == "dll"
                {
                    // Verify it's within our target directory
                    if path.starts_with(&self.target_dir) {
                        return Some(path);
                    }
                }
            }
        }

        None
    }

    /// Trace multiple profiles and merge results
    pub async fn trace_profiles(
        &self,
        project_dir: &Path,
        mode: TraceMode,
        profiles: &[String],
        feature_config: &FeatureConfig,
        custom_command: Option<&str>,
    ) -> Result<TraceResult> {
        let mut merged = TraceResult::default();

        for (idx, profile) in profiles.iter().enumerate() {
            if profiles.len() > 1 && custom_command.is_none() {
                println!("📦 Profile {}/{}: {}", idx + 1, profiles.len(), profile);
            }

            let result = self
                .trace(
                    project_dir,
                    mode,
                    Some(profile),
                    feature_config,
                    custom_command,
                )
                .await
                .with_context(|| format!("Failed to trace profile: {profile}"))?;

            merged.used_artifacts.extend(result.used_artifacts);
        }

        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_artifact_path() {
        let parser = TraceParser::new(PathBuf::from("/project/target"));

        // Test rlib extraction
        let line =
            r#"max output mtime for "foo" is "/project/target/debug/deps/libfoo-abc123.rlib" 123s"#;
        let path = parser.extract_artifact_path(line);
        assert_eq!(
            path,
            Some(PathBuf::from(
                "/project/target/debug/deps/libfoo-abc123.rlib"
            ))
        );

        // Test rmeta extraction
        let line =
            r#"max dep mtime for "bar" is "/project/target/debug/deps/libbar-xyz789.rmeta" 456s"#;
        let path = parser.extract_artifact_path(line);
        assert_eq!(
            path,
            Some(PathBuf::from(
                "/project/target/debug/deps/libbar-xyz789.rmeta"
            ))
        );

        // Test non-matching line
        let line = "Some other log line without artifacts";
        let path = parser.extract_artifact_path(line);
        assert_eq!(path, None);

        // Test path outside target directory
        let line = r#"max output mtime is "/other/path/libfoo-abc123.rlib" 123s"#;
        let path = parser.extract_artifact_path(line);
        assert_eq!(path, None);
    }
}
