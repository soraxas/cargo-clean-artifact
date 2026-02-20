//! Centralized artifact line formatting with consistent ANSI coloring.
//!
//! Two themes share the same structure but differ in profile and crate-name color:
//!   - `IN_USE`:    green profile, bold-cyan crate name
//!   - `TO_REMOVE`: orange profile, bold-yellow crate name
//!
//! Extensions use a fixed round-robin palette (same across both themes).

use crate::crate_deps::format_bytes;

// ── Extension palette (shared, deterministic) ────────────────────────────────

const EXT_PALETTE: &[(&str, &str)] = &[
    ("rlib", "\x1b[38;5;75m"),   // cornflower blue
    ("rmeta", "\x1b[38;5;110m"), // light steel blue
    ("so", "\x1b[38;5;141m"),    // medium orchid
    ("dylib", "\x1b[38;5;179m"), // burlywood
    ("dll", "\x1b[38;5;167m"),   // indian red
    ("dwo", "\x1b[38;5;246m"),   // gray
    ("o", "\x1b[38;5;244m"),     // medium gray
    ("d", "\x1b[38;5;241m"),     // dark gray
];

// Fallback cycle for unknown extensions
const FALLBACK_EXT: &[&str] = &[
    "\x1b[38;5;156m", // light green
    "\x1b[38;5;215m", // peach
    "\x1b[38;5;219m", // pink
    "\x1b[38;5;159m", // light cyan
];

fn ext_color(ext: &str) -> &'static str {
    if let Some((_, color)) = EXT_PALETTE.iter().find(|(e, _)| *e == ext) {
        return color;
    }
    // Stable hash → pick from fallback
    let idx = ext
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_add(b as usize));
    FALLBACK_EXT[idx % FALLBACK_EXT.len()]
}

// ── Shared dim/neutral codes ──────────────────────────────────────────────────

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const HASH_C: &str = "\x1b[38;5;242m"; // dark gray
const SEP_C: &str = "\x1b[38;5;245m"; // mid-gray
const SIZE_C: &str = "\x1b[32m"; // green
const IDX_C: &str = "\x1b[2m"; // dim index

// ── Profile palette (round-robin, stable per name) ───────────────────────────

const PROFILE_PALETTE: &[&str] = &[
    "\x1b[38;5;78m",  // medium sea green
    "\x1b[38;5;75m",  // cornflower blue
    "\x1b[38;5;141m", // medium orchid
    "\x1b[38;5;215m", // peach/orange
    "\x1b[38;5;156m", // light green
    "\x1b[38;5;219m", // pink
    "\x1b[38;5;159m", // light cyan
    "\x1b[38;5;179m", // burlywood
];

/// Returns a stable ANSI color for a given profile string (round-robin by hash).
pub fn profile_color(profile: &str) -> &'static str {
    let idx = profile
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_add(b as usize));
    PROFILE_PALETTE[idx % PROFILE_PALETTE.len()]
}

// ── Theme ─────────────────────────────────────────────────────────────────────

pub struct Theme {
    /// ANSI code for the profile segment (e.g. `debug`, `release`)
    pub profile: &'static str,
    /// ANSI code for the crate name (the sharp part after stripping `lib`)
    pub crate_name: &'static str,
    /// Icon shown instead of the numeric index. `None` → show `N.` number.
    pub line_icon: Option<&'static str>,
}

pub const IN_USE: Theme = Theme {
    profile: "\x1b[38;5;78m", // medium sea green
    crate_name: "\x1b[1;96m", // bold bright cyan
    line_icon: None,
};

pub const TO_REMOVE: Theme = Theme {
    profile: "\x1b[38;5;208m",            // orange
    crate_name: "\x1b[1;93m",             // bold bright yellow
    line_icon: Some("\x1b[31m🗑\x1b[0m "), // red trash icon
};

// ── Filename parser ───────────────────────────────────────────────────────────

struct ParsedArtifact<'a> {
    lib_prefix: bool,
    crate_name: &'a str,
    hash: Option<&'a str>,
    /// The full extension string (everything after first `.`)
    ext: Option<&'a str>,
}

fn parse_artifact(filename: &str) -> ParsedArtifact<'_> {
    let (lib_prefix, rest) = if let Some(s) = filename.strip_prefix("lib") {
        (true, s)
    } else {
        (false, filename)
    };

    // Split on first '.' to get stem + extension(s)
    let (stem, ext) = match rest.find('.') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };

    // Split stem on last '-' to separate crate_name and hash
    match stem.rfind('-') {
        Some(i) => ParsedArtifact {
            lib_prefix,
            crate_name: &stem[..i],
            hash: Some(&stem[i + 1..]),
            ext,
        },
        None => ParsedArtifact {
            lib_prefix,
            crate_name: stem,
            hash: None,
            ext,
        },
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Format a single artifact line with ANSI colors.
///
/// # Arguments
/// * `index` – 1-based position number
/// * `profile` – e.g. `"debug"`, `"release"`, `"wasm32-unknown-unknown/wasm-dev"`
/// * `filename` – bare filename, e.g. `"libregex_automata-0b81c4f47542bec9.rlib"`
/// * `size` – file size in bytes
/// * `used_by` – optional slice of crate names that reference this artifact (in-use only)
/// * `theme` – `&IN_USE` or `&TO_REMOVE`
pub fn format_artifact_line(
    index: usize,
    profile: &str,
    filename: &str,
    size: u64,
    used_by: Option<&[&str]>,
    theme: &Theme,
) -> String {
    let p = parse_artifact(filename);

    // Coloured filename
    let mut name = String::new();
    if p.lib_prefix {
        name.push_str(&format!("{DIM}lib{RESET}"));
    }
    name.push_str(&format!("{}{}{RESET}", theme.crate_name, p.crate_name));
    if let Some(hash) = p.hash {
        name.push_str(&format!("{SEP_C}-{RESET}"));
        name.push_str(&format!("{HASH_C}{hash}{RESET}"));
    }
    if let Some(ext) = p.ext {
        // Color the final component of a multi-part extension (e.g. "rcgu.dwo" → "dwo")
        let ext_key = ext.rsplit('.').next().unwrap_or(ext);
        name.push_str(&format!("{DIM}.{RESET}"));
        name.push_str(&format!("{}{ext}{RESET}", ext_color(ext_key)));
    }

    // Optional used-by annotation
    let used_by_str = used_by
        .map(|users| {
            let mut v = users.to_vec();
            v.sort();
            v.dedup();
            v.truncate(3);
            let suffix = if users.len() > 3 {
                format!(", +{}", users.len() - 3)
            } else {
                String::new()
            };
            format!(" {DIM}← {}{suffix}{RESET}", v.join(", "))
        })
        .unwrap_or_default();

    let index_str = match theme.line_icon {
        Some(icon) => format!("  {icon}"),
        None => format!("  {IDX_C}{index:>3}.{RESET}"),
    };

    format!(
        "{index_str} {}{profile}{RESET} {name} {SIZE_C}({size_str}){RESET}{used_by_str}",
        theme.profile,
        size_str = format_bytes(size),
    )
}

/// Format the "… and N more files" trailer line.
pub fn format_more_line(remaining: usize, label: &str) -> String {
    format!("  {DIM}  … and {remaining} more {label}{RESET}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_artifact ────────────────────────────────────────────────────────

    #[test]
    fn parse_artifact_rlib() {
        let p = parse_artifact("libregex_automata-0b81c4f47542bec9.rlib");
        assert!(p.lib_prefix);
        assert_eq!(p.crate_name, "regex_automata");
        assert_eq!(p.hash, Some("0b81c4f47542bec9"));
        assert_eq!(p.ext, Some("rlib"));
    }

    #[test]
    fn parse_artifact_rmeta() {
        let p = parse_artifact("libserde-abc123.rmeta");
        assert!(p.lib_prefix);
        assert_eq!(p.crate_name, "serde");
        assert_eq!(p.hash, Some("abc123"));
        assert_eq!(p.ext, Some("rmeta"));
    }

    #[test]
    fn parse_artifact_so_no_lib() {
        let p = parse_artifact("libserde_derive-abc.so");
        assert!(p.lib_prefix);
        assert_eq!(p.crate_name, "serde_derive");
    }

    #[test]
    fn parse_artifact_no_hash() {
        let p = parse_artifact("libserde.rlib");
        assert!(p.lib_prefix);
        assert_eq!(p.crate_name, "serde");
        assert_eq!(p.hash, None);
        assert_eq!(p.ext, Some("rlib"));
    }

    #[test]
    fn parse_artifact_no_lib_prefix() {
        let p = parse_artifact("cargo_clean-abc.d");
        assert!(!p.lib_prefix);
        assert_eq!(p.crate_name, "cargo_clean");
        assert_eq!(p.hash, Some("abc"));
    }

    #[test]
    fn parse_artifact_multi_ext() {
        // e.g. foo-HASH.foo.cgu.00.rcgu.dwo
        let p = parse_artifact("foo-HASH.foo.cgu.00.rcgu.dwo");
        assert!(!p.lib_prefix);
        assert_eq!(p.crate_name, "foo");
        assert_eq!(p.hash, Some("HASH"));
        assert_eq!(p.ext, Some("foo.cgu.00.rcgu.dwo"));
    }

    // ── profile_color ─────────────────────────────────────────────────────────

    #[test]
    fn profile_color_is_stable() {
        // Same name always maps to the same color
        assert_eq!(profile_color("debug"), profile_color("debug"));
        assert_eq!(profile_color("release"), profile_color("release"));
    }

    #[test]
    fn profile_color_different_profiles_can_differ() {
        // "debug" and "release" should hash to different palette slots
        // (This checks the round-robin logic is non-trivial)
        let colors: Vec<&str> = [
            "debug",
            "release",
            "wasm-dev",
            "wasm-release",
            "wasm32-unknown-unknown/wasm-dev",
        ]
        .iter()
        .map(|p| profile_color(p))
        .collect();
        // At least two distinct colors among the 5 profiles
        let unique: std::collections::HashSet<_> = colors.iter().collect();
        assert!(
            unique.len() >= 2,
            "expected distinct colors for different profiles"
        );
    }

    // ── format_artifact_line ──────────────────────────────────────────────────

    #[test]
    fn format_artifact_line_in_use_contains_crate_name() {
        let line = format_artifact_line(1, "debug", "libserde-abc123.rlib", 1024, None, &IN_USE);
        // Should contain the crate name somewhere (stripped of ANSI)
        assert!(line.contains("serde"), "line: {line:?}");
        assert!(line.contains("debug"), "line: {line:?}");
        assert!(line.contains("1.00 KiB"), "line: {line:?}");
    }

    #[test]
    fn format_artifact_line_to_remove_has_no_index() {
        let line = format_artifact_line(3, "release", "libfoo-hash.rmeta", 2048, None, &TO_REMOVE);
        // TO_REMOVE uses an icon, not "  3."
        assert!(!line.contains("  3."), "should not have index: {line:?}");
        assert!(line.contains("foo"), "line: {line:?}");
    }

    #[test]
    fn format_artifact_line_in_use_shows_index() {
        let line = format_artifact_line(7, "debug", "libfoo-hash.rlib", 512, None, &IN_USE);
        assert!(line.contains("7."), "should contain index: {line:?}");
    }

    #[test]
    fn format_artifact_line_used_by_annotation() {
        let line = format_artifact_line(
            1,
            "debug",
            "libserde-abc.rlib",
            100,
            Some(&["my_crate"]),
            &IN_USE,
        );
        assert!(line.contains("my_crate"), "line: {line:?}");
    }

    #[test]
    fn format_more_line_output() {
        let line = format_more_line(42, "files");
        assert!(line.contains("42"), "line: {line:?}");
        assert!(line.contains("files"), "line: {line:?}");
    }
}
