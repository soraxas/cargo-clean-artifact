use std::io::{self, IsTerminal};

use anstyle::{AnsiColor, Style};

use crate::crate_deps::{format_bytes, paint};

use super::stats::CleanupStats;

pub(super) fn print_profile_breakdown(stats: &CleanupStats) {
    if stats.per_profile.is_empty() {
        return;
    }
    let color = io::stdout().is_terminal();
    let header_style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();

    println!("{}", paint(color, "By profile:", header_style));
    for (profile, profile_stat) in &stats.per_profile {
        let total_suffix = if profile_stat.total_dir_bytes > 0 {
            format!(
                "  \x1b[2m[{} kept / {} total dir]\x1b[0m",
                format_bytes(profile_stat.used_bytes),
                format_bytes(profile_stat.total_dir_bytes),
            )
        } else {
            String::new()
        };
        let to_remove = if profile_stat.files > 0 {
            format!(
                "  \x1b[31m-{} files ({})\x1b[0m",
                profile_stat.files,
                format_bytes(profile_stat.bytes)
            )
        } else {
            String::new()
        };
        println!(
            "  {}{}:\x1b[0m\x1b[0m{}{}",
            crate::theme::profile_color(profile),
            profile,
            total_suffix,
            to_remove,
        );
    }
    println!();
}

pub(super) fn print_detailed_summary(stats: &CleanupStats) {
    let color = io::stdout().is_terminal();
    let header_style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
    let _profile_style = Style::new().fg_color(Some(AnsiColor::Magenta.into()));
    let size_style = Style::new().fg_color(Some(AnsiColor::Green.into()));
    let dim_style = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));

    println!();
    println!(
        "{} {}{}",
        paint(color, "📊 Summary:", header_style),
        paint(
            color,
            format!(
                "{} files ({}) can be removed",
                stats.files,
                format_bytes(stats.bytes)
            ),
            size_style
        ),
        if stats.used_bytes > 0 {
            paint(
                color,
                format!("  •  {} in use", format_bytes(stats.used_bytes)),
                dim_style,
            )
        } else {
            String::new()
        }
    );
    println!();

    // Show top 10 files to be removed
    if color {
        // Red powerline-style header: red bg + bold white text + right-pointing arrow cap
        println!("\x1b[41;1;97m 🗑  Files to remove: \x1b[0m\x1b[31m\u{e0b0}\x1b[0m");
    } else {
        println!("{}", paint(color, "Files to remove:", header_style));
    }
    let mut files_sorted = stats.files_to_remove.clone();
    files_sorted.sort_by_key(|f| std::cmp::Reverse(f.size));

    for (i, file_info) in files_sorted.iter().take(10).enumerate() {
        let filename = file_info
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        println!(
            "{}",
            crate::theme::format_artifact_line(
                i + 1,
                &file_info.profile,
                filename,
                file_info.size,
                None,
                &crate::theme::TO_REMOVE,
            )
        );
    }

    if files_sorted.len() > 10 {
        println!(
            "{}",
            crate::theme::format_more_line(files_sorted.len() - 10, "files")
        );
    }
}

pub(super) fn print_removal_summary(stats: &CleanupStats) {
    let color = io::stdout().is_terminal();
    let success_style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    println!();
    println!(
        "{} {} files ({}) across {} crates",
        paint(color, "✓ Removed", success_style),
        paint(color, stats.files.to_string(), accent_style),
        paint(color, format_bytes(stats.bytes), accent_style),
        paint(color, stats.per_crate.len().to_string(), accent_style),
    );

    print_top_crates(stats, color);
    print_errors(stats, color);
}

pub(super) fn print_dry_run_summary(stats: &CleanupStats) {
    let color = io::stdout().is_terminal();
    let dry_run_style = Style::new().fg_color(Some(AnsiColor::Yellow.into()));
    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    println!();
    println!(
        "{} would remove {} files ({}) across {} crates",
        paint(color, "Dry-run:", dry_run_style),
        paint(color, stats.files.to_string(), accent_style),
        paint(color, format_bytes(stats.bytes), accent_style),
        paint(color, stats.per_crate.len().to_string(), accent_style),
    );

    print_top_crates(stats, color);
}

fn print_top_crates(stats: &CleanupStats, color: bool) {
    let mut crates: Vec<_> = stats.per_crate.iter().collect();
    crates.sort_by_key(|(name, stat)| (std::cmp::Reverse(stat.bytes), name.to_string()));

    let accent_style = Style::new().fg_color(Some(AnsiColor::Cyan.into()));

    const MAX_CRATES: usize = 20;
    for (name, stat) in crates.iter().take(MAX_CRATES) {
        println!(
            "  - {}: {} files ({})",
            paint(color, name, accent_style),
            paint(color, stat.files.to_string(), accent_style),
            paint(color, format_bytes(stat.bytes), accent_style)
        );
    }

    if crates.len() > MAX_CRATES {
        println!(
            "  ... and {} more crates",
            paint(color, (crates.len() - MAX_CRATES).to_string(), accent_style)
        );
    }
}

pub(super) fn print_errors(stats: &CleanupStats, color: bool) {
    if stats.errors.is_empty() {
        return;
    }

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
            format!("({} files)", stats.errors.len()),
            error_accent_style
        )
    );
    for ((crate_name, flavor, file), error) in stats.errors.iter() {
        println!(
            "  {} [{}]: {} -> {}",
            paint(color, crate_name, error_crate_style),
            paint(color, flavor, error_flavor_style),
            paint(color, file, error_file_style),
            paint(color, format!("{error}"), error_headline_style),
        );
    }
    println!();
}
