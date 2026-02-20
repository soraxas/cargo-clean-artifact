use anstyle::{Reset, Style};
use std::path::Path;

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
