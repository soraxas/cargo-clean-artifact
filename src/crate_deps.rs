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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn crate_key_strips_lib_prefix_and_hash() {
        assert_eq!(crate_key(&PathBuf::from("libserde-abc123.rlib")), "serde");
        assert_eq!(
            crate_key(&PathBuf::from("libregex_automata-0b81c4f4.rlib")),
            "regex_automata"
        );
        assert_eq!(crate_key(&PathBuf::from("libfoo-HASH.rmeta")), "foo");
    }

    #[test]
    fn crate_key_no_lib_prefix() {
        assert_eq!(
            crate_key(&PathBuf::from("cargo_clean-abc.d")),
            "cargo_clean"
        );
    }

    #[test]
    fn crate_key_no_hash() {
        assert_eq!(crate_key(&PathBuf::from("libserde.rlib")), "serde");
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(format_bytes(1536), "1.50 KiB");
    }
}
