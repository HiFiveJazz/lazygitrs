use std::env;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

use super::cmd::CmdBuilder;

pub struct Platform;

impl Platform {
    pub fn open_file(path: &str) -> Result<()> {
        let cmd = if cfg!(target_os = "macos") {
            CmdBuilder::new("open").arg(path)
        } else if cfg!(target_os = "windows") {
            CmdBuilder::new("cmd").args(&["/c", "start", "", path])
        } else {
            CmdBuilder::new("xdg-open").arg(path)
        };

        cmd.run()?;
        Ok(())
    }

    pub fn copy_to_clipboard(text: &str) -> Result<()> {
        if cfg!(target_os = "macos") {
            return copy_with("pbcopy", &[], text);
        }

        if cfg!(target_os = "windows") {
            return copy_with("clip.exe", &[], text).or_else(|_| copy_with("clip", &[], text));
        }

        let mut last_error = None;
        for (program, args) in linux_clipboard_candidates(false) {
            if !command_exists(program) {
                continue;
            }
            match copy_with(program, args, text) {
                Ok(()) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }

        if let Some(err) = last_error {
            bail!("No usable clipboard helper succeeded: {err}");
        }
        bail!(linux_clipboard_missing_message())
    }

    pub fn read_from_clipboard() -> Result<String> {
        if cfg!(target_os = "macos") {
            return read_with("pbpaste", &[]);
        }

        if cfg!(target_os = "windows") {
            if command_exists("powershell.exe") {
                return read_with(
                    "powershell.exe",
                    &["-NoProfile", "-Command", "Get-Clipboard -Raw"],
                );
            }
            if command_exists("powershell") {
                return read_with(
                    "powershell",
                    &["-NoProfile", "-Command", "Get-Clipboard -Raw"],
                );
            }
            bail!("No Windows clipboard reader found (expected powershell.exe)");
        }

        let mut last_error = None;
        for (program, args) in linux_clipboard_candidates(true) {
            if !command_exists(program) {
                continue;
            }
            match read_with(program, args) {
                Ok(text) => return Ok(text),
                Err(err) => last_error = Some(err),
            }
        }

        if let Some(err) = last_error {
            bail!("No usable clipboard helper succeeded: {err}");
        }
        bail!(linux_clipboard_missing_message())
    }
}

fn copy_with(program: &str, args: &[&str], text: &str) -> Result<()> {
    CmdBuilder::new(program)
        .args(args)
        .stdin(text.to_string())
        .run_expecting_success()?;
    Ok(())
}

fn read_with(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        bail!(
            "Clipboard command failed (exit {}): {} {}",
            output.status.code().unwrap_or(-1),
            program,
            args.join(" ")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn linux_clipboard_candidates(read: bool) -> Vec<(&'static str, &'static [&'static str])> {
    linux_clipboard_candidates_for(
        read,
        env::var_os("WAYLAND_DISPLAY").is_some(),
        env::var_os("DISPLAY").is_some(),
    )
}

fn linux_clipboard_candidates_for(
    read: bool,
    wayland: bool,
    x11: bool,
) -> Vec<(&'static str, &'static [&'static str])> {
    const WL_COPY_ARGS: &[&str] = &[];
    const WL_PASTE_ARGS: &[&str] = &["--no-newline"];
    const XCLIP_COPY_ARGS: &[&str] = &["-selection", "clipboard"];
    const XCLIP_PASTE_ARGS: &[&str] = &["-selection", "clipboard", "-o"];
    const XSEL_COPY_ARGS: &[&str] = &["--clipboard", "--input"];
    const XSEL_PASTE_ARGS: &[&str] = &["--clipboard", "--output"];

    let wl = if read {
        ("wl-paste", WL_PASTE_ARGS)
    } else {
        ("wl-copy", WL_COPY_ARGS)
    };
    let xclip = if read {
        ("xclip", XCLIP_PASTE_ARGS)
    } else {
        ("xclip", XCLIP_COPY_ARGS)
    };
    let xsel = if read {
        ("xsel", XSEL_PASTE_ARGS)
    } else {
        ("xsel", XSEL_COPY_ARGS)
    };

    if wayland {
        vec![wl, xclip, xsel]
    } else if x11 {
        vec![xclip, xsel, wl]
    } else {
        // SSH/tmux and unusual launchers can omit display variables while the
        // helper still has enough environment to connect, so try all of them.
        vec![wl, xclip, xsel]
    }
}

fn command_exists(program: &str) -> bool {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file();
    }

    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

fn linux_clipboard_missing_message() -> &'static str {
    if env::var_os("WAYLAND_DISPLAY").is_some() {
        "No clipboard helper found. Install wl-clipboard (preferred on Wayland), xclip, or xsel."
    } else if env::var_os("DISPLAY").is_some() {
        "No clipboard helper found. Install xclip or xsel (X11), or wl-clipboard for Wayland."
    } else {
        "No clipboard helper found. Install wl-clipboard, xclip, or xsel."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_prefers_wl_clipboard() {
        let copy = linux_clipboard_candidates_for(false, true, true);
        let paste = linux_clipboard_candidates_for(true, true, true);
        assert_eq!(copy[0].0, "wl-copy");
        assert_eq!(paste[0].0, "wl-paste");
    }

    #[test]
    fn x11_prefers_xclip_then_xsel() {
        let copy = linux_clipboard_candidates_for(false, false, true);
        let paste = linux_clipboard_candidates_for(true, false, true);
        assert_eq!(
            copy.iter().map(|item| item.0).collect::<Vec<_>>(),
            vec!["xclip", "xsel", "wl-copy"]
        );
        assert_eq!(
            paste.iter().map(|item| item.0).collect::<Vec<_>>(),
            vec!["xclip", "xsel", "wl-paste"]
        );
    }
}
