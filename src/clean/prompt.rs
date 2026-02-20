use std::io::{self, IsTerminal, Write};

use anstyle::{AnsiColor, Style};
use anyhow::Result;
use console::{Key, Term};

use crate::crate_deps::{format_bytes, paint};

use super::stats::CleanupStats;

/// Which categories the user chose to remove in the step-by-step prompt.
#[derive(Default)]
pub(super) struct RemovalSelection {
    pub(super) remove_files: bool,
    pub(super) remove_dirs: bool,
}

impl RemovalSelection {
    pub(super) fn any(&self) -> bool {
        self.remove_files || self.remove_dirs
    }
}

pub(super) fn ask_yes_no(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

pub(super) const PRESET_COMMANDS: &[&str] = &[
    "cargo build",
    "cargo build --release",
    "cargo build --all-features",
    "cargo build --all-features --release",
    "trunk build",
    "trunk build --release",
    "mise run build",
];

/// Interactive arrow-key menu to pick a build command when `-c` is not given.
/// Returns `None` if the user cancels (Esc/q) or stdin is not a TTY.
pub(super) fn select_command_interactive() -> Result<Option<String>> {
    if !io::stderr().is_terminal() {
        return Ok(None);
    }

    let term = Term::stderr();
    let color = true;

    let header_style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
    let sel_style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    let dim_style = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));
    let hint_style = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));

    // Build item list: presets + "Enter custom command"
    let mut items: Vec<String> = PRESET_COMMANDS.iter().map(|s| s.to_string()).collect();
    items.push("✏  Enter custom command…".to_string());
    let custom_idx = items.len() - 1;

    let mut selected: usize = 0;
    let n = items.len();

    // Draw the menu, returning how many lines were written
    let draw = |term: &Term, selected: usize| -> Result<usize> {
        let mut lines = 0;
        let header = format!(
            "\n{}\n",
            paint(
                color,
                "No -c command given. Select a build command:",
                header_style
            )
        );
        term.write_str(&header)?;
        lines += 2; // blank line + header line

        for (i, item) in items.iter().enumerate() {
            if i == selected {
                term.write_line(&format!(
                    "  {} {}",
                    paint(color, "❯", sel_style),
                    paint(color, item, sel_style),
                ))?;
            } else {
                term.write_line(&format!("    {}", paint(color, item, dim_style),))?;
            }
            lines += 1;
        }
        term.write_line(&format!(
            "\n  {}",
            paint(
                color,
                "↑/↓ to move  •  Enter to select  •  Esc/q to abort",
                hint_style
            ),
        ))?;
        lines += 2;
        Ok(lines)
    };

    term.hide_cursor()?;
    let mut drawn_lines = draw(&term, selected)?;

    let result = loop {
        match term.read_key()? {
            Key::ArrowUp | Key::Char('k') => {
                selected = if selected == 0 { n - 1 } else { selected - 1 };
            }
            Key::ArrowDown | Key::Char('j') => {
                selected = (selected + 1) % n;
            }
            Key::Enter => break Some(selected),
            Key::Escape | Key::Char('q') => break None,
            _ => continue,
        }
        term.clear_last_lines(drawn_lines)?;
        drawn_lines = draw(&term, selected)?;
    };

    term.clear_last_lines(drawn_lines)?;
    term.show_cursor()?;

    match result {
        None => Ok(None),
        Some(idx) if idx == custom_idx => {
            // Ask user to type a command
            eprint!("{}", paint(color, "  Build command: ", Style::new().bold()));
            io::stderr().flush()?;
            let mut cmd = String::new();
            io::stdin().read_line(&mut cmd)?;
            let cmd = cmd.trim().to_string();
            if cmd.is_empty() {
                Ok(None)
            } else {
                Ok(Some(cmd))
            }
        }
        Some(idx) => Ok(Some(items[idx].clone())),
    }
}

pub(super) fn prompt_step_by_step(stats: &CleanupStats) -> Result<RemovalSelection> {
    let color = io::stdout().is_terminal();
    let prompt_style = Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold();
    let size_style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
    let dim_style = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));
    let mut sel = RemovalSelection::default();

    // ── Step 1: stale artifact files ─────────────────────────────────────────
    if !stats.files_to_remove.is_empty() {
        let files_bytes: u64 = stats.files_to_remove.iter().map(|f| f.size).sum();
        let prompt = format!(
            "{} Remove {} stale artifact files ({})? [y/N]: ",
            paint(color, "❯", prompt_style),
            paint(color, stats.files_to_remove.len().to_string(), size_style),
            paint(color, format_bytes(files_bytes), size_style),
        );
        sel.remove_files = ask_yes_no(&prompt)?;
    }

    // ── Step 2: stale incremental dirs ───────────────────────────────────────
    if !stats.dirs_to_remove.is_empty() {
        // Show top stale incremental dirs sorted by size
        let mut sorted_dirs = stats.dirs_to_remove.clone();
        sorted_dirs.sort_by_key(|d| std::cmp::Reverse(d.size));
        let dirs_bytes: u64 = sorted_dirs.iter().map(|d| d.size).sum();

        println!();
        println!();
        println!(
            "{}",
            paint(color, "🗂  Stale incremental sessions:", Style::new().bold())
        );
        let show_n = 5.min(sorted_dirs.len());
        for dir in sorted_dirs.iter().take(show_n) {
            let name = dir.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            println!(
                "  {}  {} {}",
                paint(
                    color,
                    "🗑",
                    Style::new().fg_color(Some(AnsiColor::Red.into()))
                ),
                paint(color, name, dim_style),
                paint(color, format!("({})", format_bytes(dir.size)), size_style),
            );
        }
        if sorted_dirs.len() > show_n {
            println!(
                "  {}",
                paint(
                    color,
                    format!("… and {} more stale sessions", sorted_dirs.len() - show_n),
                    dim_style
                )
            );
        }

        let prompt = format!(
            "{} Remove {} stale incremental dirs ({})? [y/N]: ",
            paint(color, "❯", prompt_style),
            paint(color, sorted_dirs.len().to_string(), size_style),
            paint(color, format_bytes(dirs_bytes), size_style),
        );
        sel.remove_dirs = ask_yes_no(&prompt)?;
    }

    // ── Final combined confirmation ───────────────────────────────────────────
    if sel.any() {
        let mut parts: Vec<String> = Vec::new();
        let mut total_bytes = 0u64;
        if sel.remove_files {
            let b: u64 = stats.files_to_remove.iter().map(|f| f.size).sum();
            parts.push(format!("{} files", stats.files_to_remove.len()));
            total_bytes += b;
        }
        if sel.remove_dirs {
            let b: u64 = stats.dirs_to_remove.iter().map(|d| d.size).sum();
            parts.push(format!(
                "{} stale incremental dirs",
                stats.dirs_to_remove.len()
            ));
            total_bytes += b;
        }
        let desc = parts.join(" + ");
        let prompt = format!(
            "\n{} Remove {} ({})? [y/N]: ",
            paint(color, "❯", prompt_style),
            paint(color, desc, size_style),
            paint(color, format_bytes(total_bytes), size_style),
        );
        let confirmed = ask_yes_no(&prompt)?;
        if !confirmed {
            sel.remove_files = false;
            sel.remove_dirs = false;
        }
    }

    Ok(sel)
}
