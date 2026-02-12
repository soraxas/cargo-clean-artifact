use anstyle::{Reset, Style};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Debug)]
pub(crate) struct DepFile {
    pub(crate) map: HashMap<PathBuf, Vec<PathBuf>>,
}

pub(crate) async fn read_deps_dir(dir: &Path) -> Result<Vec<DepFile>> {
    let mut entries = fs::read_dir(dir).await?;
    let mut files = vec![];

    while let Some(e) = entries.next_entry().await? {
        let path = e.path();
        if path.extension().is_some_and(|ext| ext == "d") {
            let content = fs::read_to_string(&path).await?;
            let file = parse_dep_file(&content)?;
            files.push(file);
        }
    }

    Ok(files)
}

pub(crate) fn parse_dep_file(s: &str) -> Result<DepFile> {
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

pub(crate) fn paint(enabled: bool, text: impl AsRef<str>, style: Style) -> String {
    if !enabled {
        return text.as_ref().to_string();
    }
    format!("{style}{}{}", text.as_ref(), Reset)
}

pub(crate) fn crate_key(path: &Path) -> String {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // Strip common prefixes like `lib`.
    let name = name.strip_prefix("lib").unwrap_or(name);

    // Drop trailing build hash if present: libfoo-<hash>.rlib
    name.split('-').next().unwrap_or(name).to_string()
}

pub(crate) fn format_bytes(bytes: u64) -> String {
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
        format!("{bytes} B")
    }
}
