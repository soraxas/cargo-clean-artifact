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

// ── Theme ─────────────────────────────────────────────────────────────────────

pub struct Theme {
    /// ANSI code for the profile segment (e.g. `debug`, `release`)
    pub profile: &'static str,
    /// ANSI code for the crate name (the sharp part after stripping `lib`)
    pub crate_name: &'static str,
}

pub const IN_USE: Theme = Theme {
    profile: "\x1b[38;5;78m", // medium sea green
    crate_name: "\x1b[1;96m", // bold bright cyan
};

pub const TO_REMOVE: Theme = Theme {
    profile: "\x1b[38;5;208m", // orange
    crate_name: "\x1b[1;93m",  // bold bright yellow
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

    format!(
        "  {IDX_C}{index:>3}.{RESET} {}{profile}{RESET} {name} {SIZE_C}({size_str}){RESET}{used_by_str}",
        theme.profile,
        size_str = format_bytes(size),
    )
}

/// Format the "… and N more files" trailer line.
pub fn format_more_line(remaining: usize, label: &str) -> String {
    format!("  {DIM}  … and {remaining} more {label}{RESET}")
}
