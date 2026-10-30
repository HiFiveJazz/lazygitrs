mod app;
mod config;
mod git;
mod gui;
mod model;
mod os;
mod pager;
mod upgrade;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use clap::{Parser, Subcommand};

use crate::git::GitCommands;

const LOGO: &str = include_str!("../logo.txt");

#[derive(Parser)]
#[command(name = "lazygitrs", version, about = "A fast and ergonomic terminal UI for git", before_help = LOGO)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to the git repository
    #[arg(short, long)]
    path: Option<PathBuf>,

    /// Git work tree path
    #[arg(short = 'w', long = "work-tree")]
    work_tree: Option<PathBuf>,

    /// Git dir path
    #[arg(short = 'g', long = "git-dir")]
    git_dir: Option<PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Filter commits by path (file or directory), like lazygit -f
    #[arg(short = 'f', long = "filter", value_name = "PATH")]
    filter_path: Option<PathBuf>,

    /// Launch directly in the commits panel
    #[arg(long)]
    commits: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Upgrade lazygitrs to the latest (or a specific) version
    Upgrade {
        /// Target version (e.g. `0.0.32`) or `latest`
        target: Option<String>,
    },
}

fn parse_init_answer(answer: &str) -> Option<bool> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn read_init_answer() -> io::Result<String> {
    let mut answer = String::new();

    if io::stdin().is_terminal() {
        io::stdin().read_line(&mut answer)?;
        return Ok(answer);
    }

    #[cfg(unix)]
    {
        let tty = std::fs::OpenOptions::new().read(true).open("/dev/tty")?;
        let mut tty = io::BufReader::new(tty);
        tty.read_line(&mut answer)?;
        return Ok(answer);
    }

    #[cfg(not(unix))]
    {
        io::stdin().read_line(&mut answer)?;
        Ok(answer)
    }
}

fn initialize_repository_if_requested(path: &Path) -> anyhow::Result<bool> {
    if GitCommands::is_valid_repo(path) {
        return Ok(true);
    }

    if !path.is_dir() {
        anyhow::bail!("'{}' is not a directory", path.display());
    }

    loop {
        let mut out = os::tty::open_tui_output()?;
        write!(
            out,
            "'{}' is not a git repository. Initialize it? (y/n): ",
            path.display()
        )?;
        out.flush()?;

        let answer = match read_init_answer() {
            Ok(answer) => answer,
            Err(_) => return Ok(false),
        };

        match parse_init_answer(&answer) {
            Some(false) => return Ok(false),
            Some(true) => {
                let status = Command::new("git")
                    .args(["init", "--quiet"])
                    .current_dir(path)
                    .status()
                    .with_context(|| format!("failed to run git init in {}", path.display()))?;
                if !status.success() {
                    anyhow::bail!("git init failed in '{}'", path.display());
                }
                return Ok(true);
            }
            None => continue,
        }
    }
}

/// Restore the terminal on panic so the user isn't left in raw mode + mouse
/// capture (which makes the shell unusable — every mouse move spews escape
/// sequences into the prompt).
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Prefer /dev/tty when stdout is redirected (Helix `:insert-output`).
        let mut out =
            crate::os::tty::open_tui_output().unwrap_or_else(|_| Box::new(std::io::stdout()));
        if crate::os::tty::nested_tty_launch() {
            // Same contract as restore_terminal: Helix still owns alt-screen /
            // raw / mouse. Only undo our kitty push and hand the tty back.
            let _ = crossterm::execute!(out, crossterm::cursor::Show);
            let _ = crossterm::execute!(
                out,
                crossterm::event::PopKeyboardEnhancementFlags,
                crossterm::event::PushKeyboardEnhancementFlags(
                    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | crossterm::event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                ),
            );
            crate::os::tty::restore_foreground_tty();
        } else {
            crate::os::tty::restore_foreground_tty();
            let _ = crossterm::execute!(
                out,
                crossterm::event::DisableMouseCapture,
                crossterm::event::DisableFocusChange,
                crossterm::cursor::Show,
                crossterm::terminal::LeaveAlternateScreen,
            );
            let _ = crossterm::terminal::disable_raw_mode();
        }
        prev(info);
    }));
}

fn main() {
    // Helix `:insert-output` sets stdin=/dev/null + stdout=pipe while keeping its
    // EventStream on /dev/tty. Detect that, claim the tty foreground so Helix
    // can't steal keys, and draw on a separate /dev/tty handle (no stdout dup2).
    os::tty::reclaim_controlling_tty();
    install_panic_hook();
    let cli = Cli::parse();

    if let Some(Commands::Upgrade { target }) = cli.command {
        if let Err(e) = upgrade::upgrade(target.as_deref()) {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // Keep lightweight debug diagnostics dependency-free.
    os::tty::set_debug_logging(cli.debug);

    let repo_path = cli
        .path
        .or(cli.work_tree)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    match initialize_repository_if_requested(&repo_path) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }

    match app::App::new(repo_path, cli.filter_path, cli.commits) {
        Ok(app) => {
            if let Err(e) = app.run() {
                eprintln!("Error: {:#}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_commits_flag() {
        let cli = Cli::parse_from(["lazygitrs", "--commits"]);
        assert!(cli.commits);

        let cli_default = Cli::parse_from(["lazygitrs"]);
        assert!(!cli_default.commits);
    }

    #[test]
    fn parses_repository_init_answers() {
        assert_eq!(parse_init_answer("y"), Some(true));
        assert_eq!(parse_init_answer("YES\n"), Some(true));
        assert_eq!(parse_init_answer("n"), Some(false));
        assert_eq!(parse_init_answer("No"), Some(false));
        assert_eq!(parse_init_answer("maybe"), None);
    }
}
