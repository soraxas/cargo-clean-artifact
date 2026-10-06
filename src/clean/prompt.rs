use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::time::SystemTime;

use anstyle::{AnsiColor, Style};
use anyhow::Result;
use console::{Key, Term};

use crate::crate_deps::{format_bytes, paint};
use crate::detect::DetectedCommand;

use super::scan::{FolderSizeEstimate, folder_size_estimate};
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

fn menu_window(total: usize, selected: usize, height: usize) -> std::ops::Range<usize> {
    let visible = height.saturating_sub(6).max(1).min(total);
    let start = selected.saturating_sub(visible / 2).min(total - visible);
    start..start + visible
}

fn menu_label(label: &str, width: usize) -> String {
    let label: String = label
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    // Four columns for the selection marker, one spare to avoid terminal wrap.
    console::truncate_str(&label, width.saturating_sub(5), "…").into_owned()
}

pub(super) fn relative_age(modified: SystemTime, now: SystemTime) -> String {
    if modified == SystemTime::UNIX_EPOCH {
        return "unknown age".to_owned();
    }
    let (seconds, future) = match now.duration_since(modified) {
        Ok(age) => (age.as_secs(), false),
        Err(error) => (error.duration().as_secs(), true),
    };
    if seconds < 60 {
        return "just now".to_owned();
    }
    let amount = if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    };
    if future {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

struct CommandChoice {
    command: Option<String>,
    summary: Option<String>,
    folder: Option<String>,
    modified: Option<SystemTime>,
}

fn choice_label(choice: &CommandChoice, now: SystemTime, newest: bool) -> String {
    let command = choice
        .command
        .as_deref()
        .unwrap_or("✏  Enter custom command…");
    match choice.modified {
        Some(modified) => format!(
            "[{}{}] {command}",
            if newest { "newest · " } else { "" },
            relative_age(modified, now)
        ),
        None => command.to_owned(),
    }
}

fn scrolling_choice_label(
    choice: &CommandChoice,
    now: SystemTime,
    newest: bool,
    width: usize,
    offset: usize,
) -> String {
    let label = choice_label(choice, now, newest);
    if offset == 0 {
        return menu_label(&label, width);
    }
    let content = choice
        .command
        .as_deref()
        .unwrap_or("✏  Enter custom command…");
    let prefix = label.strip_suffix(content).unwrap_or("");
    let remaining = width.saturating_sub(5 + console::measure_text_width(prefix));
    if remaining < 2 {
        return menu_label(prefix, width);
    }
    let offset = offset.min(content.chars().count().saturating_sub(1));
    let tail: String = content
        .chars()
        .skip(offset)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    format!(
        "{prefix}‹{}",
        console::truncate_str(&tail, remaining - 1, "…")
    )
}

// Detected commands have a canonical `cargo build` argument order. Search only
// after that prefix so recorded compiler flags cannot masquerade as features.
fn feature_range(command: &str) -> Option<std::ops::Range<usize>> {
    let cargo = if command.starts_with("cargo build ") {
        0
    } else {
        command.rfind(" cargo build ")? + 1
    };
    let start = cargo + command[cargo..].find(" --features ")? + " --features ".len();
    let end = command[start..]
        .find(char::is_whitespace)
        .map_or(command.len(), |end| start + end);
    Some(command[..start].chars().count()..command[..end].chars().count())
}

fn paint_choice_label(choice: &CommandChoice, item: &str, offset: usize, base: Style) -> String {
    let command = choice.command.as_deref().unwrap_or("");
    let Some(features) = feature_range(command) else {
        return paint(true, item, base);
    };
    let prefix = if choice.modified.is_some() {
        let Some(end) = item.find("] ") else {
            return paint(true, item, base);
        };
        item[..end + 2].chars().count()
    } else {
        0
    };
    let offset = offset.min(command.chars().count().saturating_sub(1));
    let prefix = prefix + usize::from(offset > 0);
    // Clip in plain text first; ANSI escapes must never affect scroll offsets.
    let visible = item.chars().count() - usize::from(item.ends_with('…'));
    let start = (prefix + features.start.saturating_sub(offset)).min(visible);
    let end = (prefix + features.end.saturating_sub(offset)).min(visible);
    if start >= end {
        return paint(true, item, base);
    }
    let byte_at = |index| {
        item.char_indices()
            .nth(index)
            .map_or(item.len(), |(i, _)| i)
    };
    let start = byte_at(start);
    let end = byte_at(end);
    format!(
        "{}{}{}",
        paint(true, &item[..start], base),
        paint(
            true,
            &item[start..end],
            base.fg_color(Some(AnsiColor::Yellow.into()))
        ),
        paint(true, &item[end..], base),
    )
}

fn command_choices(detected: &[(&str, &str)]) -> Vec<CommandChoice> {
    let mut choices = Vec::<CommandChoice>::new();
    for (folder, _) in detected {
        if choices.iter().any(|c| c.folder.as_deref() == Some(folder)) {
            continue;
        }
        // Input is newest-first: first occurrence orders folders, and filtering
        // preserves recency among the feature variants within each folder.
        choices.extend(
            detected
                .iter()
                .filter(|(f, _)| f == folder)
                .map(|(f, command)| CommandChoice {
                    command: Some((*command).to_owned()),
                    summary: None,
                    folder: Some((*f).to_owned()),
                    modified: None,
                }),
        );
    }
    for preset in PRESET_COMMANDS {
        if !choices.iter().any(|c| c.command.as_deref() == Some(preset)) {
            choices.push(CommandChoice {
                command: Some((*preset).to_owned()),
                summary: None,
                folder: None,
                modified: None,
            });
        }
    }
    choices.push(CommandChoice {
        command: None,
        summary: None,
        folder: None,
        modified: None,
    });
    choices
}

enum MenuRow {
    Heading {
        label: String,
        folder: Option<String>,
    },
    Choice(usize),
}

fn command_rows(choices: &[CommandChoice]) -> Vec<MenuRow> {
    let mut rows = Vec::new();
    for (index, choice) in choices.iter().enumerate() {
        if index == 0 || choices[index - 1].folder != choice.folder {
            rows.push(MenuRow::Heading {
                label: choice
                    .folder
                    .as_ref()
                    .map(|folder| format!("Detected · {folder}/"))
                    .unwrap_or_else(|| "Presets / custom".to_owned()),
                folder: choice.folder.clone(),
            });
        }
        rows.push(MenuRow::Choice(index));
    }
    rows
}

fn folder_heading(label: &str, size: Option<FolderSizeEstimate>, width: usize) -> String {
    let Some(size) = size else {
        return menu_label(label, width);
    };
    let size = if size.incomplete && size.bytes == 0 {
        "size unavailable".to_owned()
    } else {
        format!(
            "{}{} total",
            if size.incomplete { "≥" } else { "~" },
            format_bytes(size.bytes)
        )
    };
    let suffix = format!(" · {size}");
    let remaining = width.saturating_sub(5 + console::measure_text_width(&suffix));
    if remaining == 0 {
        return menu_label(&size, width);
    }
    format!("{}{}", console::truncate_str(label, remaining, "…"), suffix)
}

/// Interactive arrow-key menu to pick a build command when `-c` is not given.
/// Returns `None` if the user cancels (Esc/q) or stdin is not a TTY.
pub(super) fn select_command_interactive(detected: &[DetectedCommand]) -> Result<Option<String>> {
    if !io::stderr().is_terminal() {
        return Ok(None);
    }

    let term = Term::stderr();
    let color = true;

    let header_style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
    let sel_style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    let detected_style = Style::new().fg_color(Some(AnsiColor::BrightBlue.into()));
    let dim_style = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));
    let hint_style = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));

    // Scan once per folder, only when opening the picker; --detect stays cheap
    // and redraws/navigation never repeat the filesystem walk.
    let mut folder_sizes = HashMap::new();
    for candidate in detected {
        folder_sizes
            .entry(candidate.folder.as_str())
            .or_insert_with(|| folder_size_estimate(&candidate.folder_path));
    }

    let candidates: Vec<_> = detected
        .iter()
        .map(|c| (c.folder.as_str(), c.command.as_str()))
        .collect();
    let mut choices = command_choices(&candidates);
    for choice in &mut choices {
        if let Some(candidate) = detected.iter().find(|candidate| {
            choice.folder.as_deref() == Some(candidate.folder.as_str())
                && choice.command.as_deref() == Some(candidate.command.as_str())
        }) {
            choice.modified = Some(candidate.modified);
            choice.summary = Some(candidate.summary.clone());
        }
    }
    let rows = command_rows(&choices);
    let custom_idx = choices.len() - 1;
    let mut selected: usize = 0;
    let mut horizontal_offset: usize = 0;
    let n = choices.len();

    // Draw the menu, returning how many lines were written
    let draw = |term: &Term, selected: usize, horizontal_offset: usize| -> Result<usize> {
        let (height, width) = term.size();
        let now = SystemTime::now();
        let mut lines = 0;
        let header = format!(
            "\n{}\n",
            paint(
                color,
                menu_label(
                    &format!(
                        "Select command to trace · {}",
                        choices[selected]
                            .folder
                            .as_deref()
                            .unwrap_or("Presets / custom")
                    ),
                    width as usize,
                ),
                header_style
            )
        );
        term.write_str(&header)?;
        lines += 2; // blank line + header line
        term.write_line(&format!(
            "  {}",
            paint(
                color,
                menu_label(
                    "Fingerprint age · newest first within each folder",
                    width as usize
                ),
                hint_style
            )
        ))?;
        lines += 1;

        let selected_row = rows
            .iter()
            .position(|row| matches!(row, MenuRow::Choice(i) if *i == selected))
            .unwrap();
        for row in &rows[menu_window(rows.len(), selected_row, height as usize)] {
            let i = match row {
                MenuRow::Heading { label, folder } => {
                    let style = if folder.is_some() {
                        Style::new()
                            .fg_color(Some(AnsiColor::BrightWhite.into()))
                            .bg_color(Some(AnsiColor::Blue.into()))
                            .bold()
                    } else {
                        Style::new()
                            .fg_color(Some(AnsiColor::BrightWhite.into()))
                            .bg_color(Some(AnsiColor::BrightBlack.into()))
                            .bold()
                    };
                    let size = folder
                        .as_deref()
                        .and_then(|folder| folder_sizes.get(folder))
                        .copied();
                    let label = folder_heading(label, size, width as usize);
                    let padding =
                        (width as usize).saturating_sub(4 + console::measure_text_width(&label));
                    let band = format!("{label}{}", " ".repeat(padding));
                    term.write_line(&format!("  {}", paint(color, band, style)))?;
                    lines += 1;
                    continue;
                }
                MenuRow::Choice(index) => *index,
            };
            let item = scrolling_choice_label(
                &choices[i],
                now,
                i == 0,
                width as usize,
                if i == selected { horizontal_offset } else { 0 },
            );
            if i == selected {
                term.write_line(&format!(
                    "  {} {}",
                    paint(color, "❯", sel_style),
                    paint_choice_label(&choices[i], &item, horizontal_offset, sel_style),
                ))?;
            } else {
                let style = if choices[i].folder.is_some() {
                    detected_style
                } else {
                    dim_style
                };
                term.write_line(&format!(
                    "    {}",
                    paint_choice_label(&choices[i], &item, 0, style),
                ))?;
            }
            lines += 1;
        }
        term.write_line(&format!(
            "\n  {}",
            paint(
                color,
                menu_label(
                    &format!(
                        "↑↓ select • ←→ scroll • v full • Enter run • q quit [{}/{}]",
                        selected + 1,
                        n
                    ),
                    width as usize,
                ),
                hint_style
            ),
        ))?;
        lines += 2;
        Ok(lines)
    };

    term.hide_cursor()?;
    let mut drawn_lines = draw(&term, selected, horizontal_offset)?;

    let result = loop {
        match term.read_key()? {
            Key::ArrowUp | Key::Char('k') => {
                selected = if selected == 0 { n - 1 } else { selected - 1 };
                horizontal_offset = 0;
            }
            Key::ArrowDown | Key::Char('j') => {
                selected = (selected + 1) % n;
                horizontal_offset = 0;
            }
            Key::ArrowRight => {
                let content = choices[selected].command.as_deref().unwrap_or("");
                horizontal_offset = horizontal_offset
                    .saturating_add(12)
                    .min(content.chars().count().saturating_sub(1));
            }
            Key::ArrowLeft => {
                horizontal_offset = horizontal_offset.saturating_sub(12);
            }
            Key::Char('v') if selected != custom_idx => {
                term.clear_last_lines(drawn_lines)?;
                term.write_line("Full command (control characters escaped):")?;
                if let Some(summary) = &choices[selected].summary {
                    term.write_line(&summary.escape_debug().to_string())?;
                }
                if let Some(modified) = choices[selected].modified {
                    term.write_line(&format!(
                        "Newest matching fingerprint: {}",
                        relative_age(modified, SystemTime::now())
                    ))?;
                }
                let preview: String = choices[selected]
                    .command
                    .as_deref()
                    .unwrap()
                    .chars()
                    .map(|ch| {
                        if ch.is_control() {
                            ch.escape_default().to_string()
                        } else {
                            ch.to_string()
                        }
                    })
                    .collect();
                term.write_line(&preview)?;
                term.write_line("\nPress any key to return to the picker.")?;
                term.read_key()?;
                term.clear_screen()?;
                drawn_lines = draw(&term, selected, horizontal_offset)?;
                continue;
            }
            Key::Enter => break Some(selected),
            Key::Escape | Key::Char('q') => break None,
            _ => continue,
        }
        term.clear_last_lines(drawn_lines)?;
        drawn_lines = draw(&term, selected, horizontal_offset)?;
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
        Some(idx) => Ok(choices[idx].command.clone()),
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

#[cfg(test)]
mod menu_tests {
    use super::*;

    #[test]
    fn heading_keeps_size_visible_when_folder_name_is_shortened() {
        let estimate = FolderSizeEstimate {
            bytes: 1024 * 1024,
            incomplete: false,
        };
        let label = folder_heading(
            "Detected · wasm32-unknown-unknown/very-long-custom-profile/",
            Some(estimate),
            50,
        );
        assert!(label.ends_with("~1.00 MiB total"));
        assert!(console::measure_text_width(&label) + 4 < 50);
        let partial = folder_heading(
            "Detected · debug/",
            Some(FolderSizeEstimate {
                incomplete: true,
                ..estimate
            }),
            80,
        );
        assert!(partial.ends_with("≥1.00 MiB total"));
        let missing = folder_heading(
            "Detected · debug/",
            Some(FolderSizeEstimate {
                bytes: 0,
                incomplete: true,
            }),
            80,
        );
        assert!(missing.ends_with("size unavailable"));
    }

    #[test]
    fn fingerprint_ages_show_minutes_hours_days_and_clock_skew() {
        use std::time::Duration;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 86400);
        for (seconds, expected) in [
            (0, "just now"),
            (59, "just now"),
            (60, "1m ago"),
            (7200, "2h ago"),
            (3 * 86400, "3d ago"),
        ] {
            assert_eq!(
                relative_age(now - Duration::from_secs(seconds), now),
                expected
            );
        }
        assert_eq!(relative_age(now + Duration::from_secs(300), now), "in 5m");
        assert_eq!(relative_age(SystemTime::UNIX_EPOCH, now), "unknown age");
    }

    #[test]
    fn age_and_newest_marker_stay_visible_when_a_long_command_is_shortened() {
        use std::time::Duration;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 86400);
        let command = "cargo build --target wasm32-unknown-unknown --profile wasm-dev --no-default-features --features brp,webgpu";
        let choice = CommandChoice {
            command: Some(command.to_owned()),
            summary: None,
            folder: Some("wasm32-unknown-unknown/wasm-dev".to_owned()),
            modified: Some(now - Duration::from_secs(7200)),
        };
        let label = menu_label(&choice_label(&choice, now, true), 80);
        assert!(label.starts_with("[newest · 2h ago] cargo build"));
        assert!(console::measure_text_width(&label) + 4 < 80);
        assert!(label.ends_with('…'));
        assert_eq!(choice.command.as_deref(), Some(command));
        assert!(choice_label(&choice, now, false).starts_with("[2h ago] cargo build"));
        let preset = CommandChoice {
            command: Some("cargo build".to_owned()),
            summary: None,
            folder: None,
            modified: None,
        };
        assert_eq!(choice_label(&preset, now, false), "cargo build");
    }

    #[test]
    fn groups_candidates_by_folder_preserving_recency_within_each_group() {
        let choices = command_choices(&[
            (
                "wasm32-unknown-unknown/wasm-dev",
                "cargo build --features newest",
            ),
            ("debug", "cargo build --features native"),
            (
                "wasm32-unknown-unknown/wasm-dev",
                "cargo build --features older",
            ),
        ]);
        let detected: Vec<_> = choices
            .iter()
            .filter(|c| c.folder.is_some())
            .map(|c| (c.folder.as_deref().unwrap(), c.command.as_deref().unwrap()))
            .collect();
        assert_eq!(
            detected,
            [
                (
                    "wasm32-unknown-unknown/wasm-dev",
                    "cargo build --features newest"
                ),
                (
                    "wasm32-unknown-unknown/wasm-dev",
                    "cargo build --features older"
                ),
                ("debug", "cargo build --features native"),
            ]
        );
    }

    #[test]
    fn presets_and_custom_input_follow_detected_groups_without_duplicate_commands() {
        let choices = command_choices(&[("debug", "cargo build")]);
        assert_eq!(
            choices
                .iter()
                .filter(|c| c.command.as_deref() == Some("cargo build"))
                .count(),
            1
        );
        assert!(
            choices.iter().any(
                |c| c.folder.is_none() && c.command.as_deref() == Some("cargo build --release")
            )
        );
        let custom = choices.last().unwrap();
        assert!(custom.command.is_none() && custom.folder.is_none());
    }

    #[test]
    fn folder_headings_do_not_become_selectable_commands() {
        let choices = command_choices(&[
            ("debug", "native build"),
            ("wasm32-unknown-unknown/wasm-dev", "wasm build"),
        ]);
        let rows = command_rows(&choices);
        let headings: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                MenuRow::Heading { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            headings,
            [
                "Detected · debug/",
                "Detected · wasm32-unknown-unknown/wasm-dev/",
                "Presets / custom"
            ]
        );
        let selectable: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                MenuRow::Choice(index) => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(selectable, (0..choices.len()).collect::<Vec<_>>());
    }

    #[test]
    fn long_inferred_commands_fit_one_terminal_row() {
        let command = "cargo build --target wasm32-unknown-unknown --profile wasm-dev --no-default-features --features brp,webgpu  (inferred)";
        let label = menu_label(command, 80);
        assert!(console::measure_text_width(&label) + 4 < 80);
        assert!(label.starts_with("cargo build"));
        assert!(label.ends_with('…'));
    }

    #[test]
    fn scrolling_keeps_selected_command_visible_and_leaves_room_for_header() {
        for selected in [0, 18, 50, 99] {
            let window = menu_window(100, selected, 24);
            assert!(window.contains(&selected));
            assert!(window.len() + 5 < 24);
            assert!(window.end <= 100);
        }
    }

    #[test]
    fn labels_do_not_emit_control_characters_from_recorded_flags() {
        let label = menu_label("RUSTFLAGS='one\ntwo\tthree\u{1f}four'", 80);
        assert!(!label.chars().any(char::is_control));
    }

    #[test]
    fn horizontal_scroll_reveals_hidden_options_while_retaining_age_and_command() {
        use std::time::Duration;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(86400);
        let command = format!(
            "cargo build --features {}special_flag",
            "long_feature,".repeat(10)
        );
        let choice = CommandChoice {
            command: Some(command.clone()),
            summary: Some("features: special_flag".to_owned()),
            folder: Some("debug".to_owned()),
            modified: Some(now - Duration::from_secs(3600)),
        };
        let initial = scrolling_choice_label(&choice, now, true, 80, 0);
        assert!(initial.starts_with("[newest · 1h ago] cargo build --features"));
        assert!(!initial.contains("special_flag"));
        let scrolled = scrolling_choice_label(&choice, now, true, 80, command.chars().count() - 30);
        assert!(scrolled.starts_with("[newest · 1h ago] ‹"));
        assert!(scrolled.contains("special_flag"));
        assert!(console::measure_text_width(&scrolled) + 4 < 80);
        assert_eq!(choice.command.as_deref(), Some(command.as_str()));
    }

    #[test]
    fn features_stay_yellow_when_scrolled_and_surrounding_command_keeps_its_style() {
        let command = "cargo build --features brp,éclair,webgpu --package demo";
        let choice = CommandChoice {
            command: Some(command.to_owned()),
            summary: Some("features: brp, éclair, webgpu".to_owned()),
            folder: Some("debug".to_owned()),
            modified: Some(SystemTime::UNIX_EPOCH),
        };
        let now = SystemTime::now();
        for base in [
            Style::new().fg_color(Some(AnsiColor::Green.into())).bold(),
            Style::new().fg_color(Some(AnsiColor::BrightBlue.into())),
        ] {
            for offset in [0, 27, 48] {
                let label = scrolling_choice_label(&choice, now, true, 120, offset);
                let styled = paint_choice_label(&choice, &label, offset, base);
                assert_eq!(console::strip_ansi_codes(&styled), label);
                if offset < 48 {
                    let features = if offset == 0 {
                        "brp,éclair,webgpu"
                    } else {
                        "éclair,webgpu"
                    };
                    assert!(styled.contains(&paint(
                        true,
                        features,
                        base.fg_color(Some(AnsiColor::Yellow.into()))
                    )));
                    assert!(styled.contains(&paint(true, " --package demo", base)));
                } else {
                    assert!(!styled.contains("\x1b[33m"));
                }
            }
        }
        let clipped = scrolling_choice_label(&choice, now, false, 53, 24);
        let styled = paint_choice_label(&choice, &clipped, 24, Style::new());
        assert_eq!(console::strip_ansi_codes(&styled), clipped);
        assert!(console::measure_text_width(&styled) + 4 < 53);
    }

    #[test]
    fn recorded_compiler_flags_cannot_be_mistaken_for_feature_arguments() {
        assert!(
            feature_range("CARGO_ENCODED_RUSTFLAGS='--features fake' cargo build --profile dev")
                .is_none()
        );
        let command = "CARGO_ENCODED_RUSTFLAGS='--features fake' cargo build --features real";
        let range = feature_range(command).unwrap();
        assert_eq!(
            command
                .chars()
                .skip(range.start)
                .take(range.len())
                .collect::<String>(),
            "real"
        );
    }
}
