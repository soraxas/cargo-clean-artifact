//! Recursive mode: discover cargo projects (a `Cargo.toml` next to a `target/`)
//! under a root directory and let the user pick one to clean.

use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::SystemTime,
};

use anstyle::{AnsiColor, Style};
use anyhow::Result;
use console::{Key, Term};
use indicatif::{ProgressBar, ProgressStyle};

use crate::crate_deps::{format_bytes, paint};

use super::prompt::{ask_yes_no, relative_age};
use super::scan::folder_size_estimate;

/// Directories never worth descending into while looking for projects.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "Library", "venv", "__pycache__"];

#[derive(Debug, Clone)]
pub(super) struct Workspace {
    pub(super) dir: PathBuf,
    pub(super) target: PathBuf,
    pub(super) size: u64,
    pub(super) modified: SystemTime,
}

/// Find every directory under `root` that has both a `Cargo.toml` and a
/// `target/` directory. Symlinks and hidden directories are not followed, and
/// nothing inside a `target/` is scanned.
pub(super) fn discover(root: &Path) -> Vec<Workspace> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut has_manifest = false;
        let mut has_target = false;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() && name == "Cargo.toml" {
                has_manifest = true;
            } else if file_type.is_dir() {
                if name == "target" {
                    has_target = true;
                } else if !name.starts_with('.') && !SKIP_DIRS.contains(&&*name) {
                    pending.push(entry.path());
                }
            }
        }
        if has_manifest && has_target {
            let target = dir.join("target");
            let modified = std::fs::metadata(&target)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push(Workspace {
                dir,
                target,
                size: 0,
                modified,
            });
        }
    }
    found
}

/// Measure every target dir in parallel (this is the slow part), then sort
/// largest first.
pub(super) fn measure(workspaces: &mut [Workspace]) {
    if workspaces.is_empty() {
        return;
    }
    let bar = ProgressBar::new(workspaces.len() as u64);
    bar.set_style(
        ProgressStyle::with_template("  measuring target dirs {bar:30.cyan/blue} {pos}/{len}")
            .unwrap(),
    );
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let sizes: Vec<std::sync::Mutex<u64>> = workspaces
        .iter()
        .map(|_| std::sync::Mutex::new(0))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(ws) = workspaces.get(i) else { break };
                    *sizes[i].lock().unwrap() = folder_size_estimate(&ws.target).bytes;
                    bar.inc(1);
                }
            });
        }
    });
    bar.finish_and_clear();
    for (ws, size) in workspaces.iter_mut().zip(sizes) {
        ws.size = size.into_inner().unwrap();
    }
    workspaces.sort_by_key(|w| std::cmp::Reverse(w.size));
}

pub(super) enum Action {
    /// Run the normal trace-based cleanup for this workspace.
    SmartClean(PathBuf),
    /// Remove the whole `target/` directory.
    RemoveTarget(PathBuf),
    Quit,
}

/// Show the picker. Returns what the user chose to do with which workspace.
pub(super) fn pick(root: &Path, workspaces: &[Workspace], selected: &mut usize) -> Result<Action> {
    let term = Term::stderr();
    if !io::stderr().is_terminal() || workspaces.is_empty() {
        return Ok(Action::Quit);
    }
    let head = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
    let sel = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    let dim = Style::new().fg_color(Some(AnsiColor::BrightBlack.into()));
    let big = Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold();
    let total: u64 = workspaces.iter().map(|w| w.size).sum();
    let n = workspaces.len();
    *selected = (*selected).min(n - 1);

    let draw = |term: &Term, selected: usize| -> Result<usize> {
        let (height, width) = term.size();
        let (height, width) = (height as usize, width as usize);
        let visible = height.saturating_sub(5).max(1).min(n);
        let start = selected.saturating_sub(visible / 2).min(n - visible);
        let mut lines = 0;
        let mut out = |s: String| -> Result<()> {
            term.write_line(&s)?;
            lines += 1;
            Ok(())
        };
        out(paint(
            true,
            format!(
                "Cargo projects under {} — {} found, {} in target dirs",
                root.display(),
                n,
                format_bytes(total)
            ),
            head,
        ))?;
        let now = SystemTime::now();
        for i in start..start + visible {
            let w = &workspaces[i];
            let rel = w.dir.strip_prefix(root).unwrap_or(&w.dir);
            let rel = if rel.as_os_str().is_empty() {
                ".".to_string()
            } else {
                rel.display().to_string()
            };
            let meta = format!(
                "{:>10}  {:>8}  ",
                format_bytes(w.size),
                relative_age(w.modified, now)
            );
            let room = width.saturating_sub(meta.len() + 5);
            let path = console::truncate_str(&rel, room, "…").into_owned();
            let line = if i == selected {
                paint(true, format!("❯ {meta}{path}"), sel)
            } else {
                format!("  {}{}", paint(true, &meta, big), path)
            };
            out(line)?;
        }
        out(paint(
            true,
            "↑/↓ move · enter smart clean (trace build) · x remove whole target/ · q quit",
            dim,
        ))?;
        Ok(lines)
    };

    term.hide_cursor()?;
    let mut drawn = draw(&term, *selected)?;
    let action = loop {
        match term.read_key()? {
            Key::ArrowUp | Key::Char('k') => *selected = (*selected + n - 1) % n,
            Key::ArrowDown | Key::Char('j') => *selected = (*selected + 1) % n,
            Key::Enter => break Action::SmartClean(workspaces[*selected].dir.clone()),
            Key::Char('x') => break Action::RemoveTarget(workspaces[*selected].dir.clone()),
            Key::Escape | Key::Char('q') => break Action::Quit,
            _ => continue,
        }
        term.clear_last_lines(drawn)?;
        drawn = draw(&term, *selected)?;
    };
    term.clear_last_lines(drawn)?;
    term.show_cursor()?;
    Ok(action)
}

/// Delete `<dir>/target` after confirmation. Returns bytes freed.
pub(super) fn remove_target(ws: &Workspace) -> Result<u64> {
    let prompt = format!(
        "Remove the ENTIRE {} ({})? [y/N]: ",
        ws.target.display(),
        format_bytes(ws.size)
    );
    if !ask_yes_no(&prompt)? {
        return Ok(0);
    }
    std::fs::remove_dir_all(&ws.target)?;
    Ok(ws.size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project(root: &Path, rel: &str, with_target: bool) {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        if with_target {
            fs::create_dir_all(dir.join("target/debug")).unwrap();
        }
    }

    #[test]
    fn discovers_only_projects_with_target_and_skips_target_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        project(r, "a", true);
        project(r, "group/b", true);
        project(r, "no_target", false);
        project(r, "a/target/debug/nested", true); // inside target: ignored
        project(r, "node_modules/x", true); // skipped dir
        project(r, ".hidden/y", true); // hidden dir
        let mut dirs: Vec<_> = discover(r)
            .into_iter()
            .map(|w| w.dir.strip_prefix(r).unwrap().to_path_buf())
            .collect();
        dirs.sort();
        assert_eq!(dirs, vec![PathBuf::from("a"), PathBuf::from("group/b")]);
    }

    #[test]
    fn measure_sorts_largest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        project(r, "small", true);
        project(r, "large", true);
        fs::write(r.join("large/target/debug/blob"), vec![1u8; 200_000]).unwrap();
        fs::write(r.join("small/target/debug/blob"), vec![1u8; 10]).unwrap();
        let mut ws = discover(r);
        measure(&mut ws);
        assert!(ws[0].dir.ends_with("large"));
        assert!(ws[0].size > ws[1].size);
    }
}
