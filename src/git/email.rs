use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::GitCommands;

static EMAIL_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempPatchDir(PathBuf);

impl TempPatchDir {
    fn create() -> Result<Self> {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for _ in 0..16 {
            let counter = EMAIL_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("lazygitrs-send-email-{pid}-{now}-{counter}"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("Failed to create {}", path.display()));
                }
            }
        }

        bail!("Failed to create a unique temporary directory for git send-email")
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPatchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEmailRecipients {
    pub to: String,
    pub cc: String,
}

impl GitCommands {
    /// Read the repository's default send-email recipients for the compose UI.
    /// Missing config is normal and produces an empty field.
    pub fn send_email_defaults(&self) -> (String, String) {
        let identity = self.send_email_config_values("sendemail.identity");
        if !identity.is_empty() {
            let identity = identity.lines().next().unwrap_or_default().trim();
            let identity_to = self.send_email_config_values(&format!("sendemail.{identity}.to"));
            let identity_cc = self.send_email_config_values(&format!("sendemail.{identity}.cc"));
            return (
                if identity_to.is_empty() {
                    self.send_email_config_values("sendemail.to")
                } else {
                    identity_to
                },
                if identity_cc.is_empty() {
                    self.send_email_config_values("sendemail.cc")
                } else {
                    identity_cc
                },
            );
        }

        (
            self.send_email_config_values("sendemail.to"),
            self.send_email_config_values("sendemail.cc"),
        )
    }

    fn send_email_config_values(&self, key: &str) -> String {
        self.git_cmd()
            .args(&["config", "--get-all", key])
            .run()
            .ok()
            .filter(|result| result.success)
            .map(|result| {
                result
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    }

    fn format_commit_email_patch(&self, hash: &str) -> Result<(TempPatchDir, String)> {
        let temp_dir = TempPatchDir::create()?;
        let output_dir = temp_dir.path().to_string_lossy().to_string();

        self.git_cmd()
            .args(&[
                "format-patch",
                "-1",
                "--output-directory",
                &output_dir,
                hash,
            ])
            .run_expecting_success()?;

        let patch_path = fs::read_dir(temp_dir.path())?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("patch"))
            .context("git format-patch did not create a .patch file")?;

        Ok((temp_dir, patch_path.to_string_lossy().to_string()))
    }

    fn send_email_command(
        &self,
        patch_path: &str,
        to: &str,
        cc: &str,
        dry_run: bool,
        resolved_cc: bool,
    ) -> crate::os::cmd::CmdBuilder {
        let mut cmd = self
            .git_cmd()
            .arg("send-email")
            .arg("--confirm=never")
            // The compose fields are authoritative. Static configured lists are
            // pre-filled into the UI and must not reappear after the user clears
            // them. Bcc is intentionally disabled until lazygitrs has a visible
            // Bcc field; recipients should never be hidden from the compose UI.
            .arg("--no-to")
            .arg("--no-cc")
            .arg("--no-bcc");

        // The preflight pass has already expanded author/trailer/ccCmd recipients
        // into the visible Cc field. Suppress a second round on the real send so
        // the message cannot silently regain an auto-CC after the user reviewed it.
        if resolved_cc {
            cmd = cmd
                .arg("--suppress-cc=all")
                .arg("--no-signed-off-by-cc")
                .arg("--suppress-from");
        }
        if dry_run {
            cmd = cmd.arg("--dry-run");
        }
        if !to.trim().is_empty() {
            cmd = cmd.arg(&format!("--to={}", to.trim()));
        }
        if !cc.trim().is_empty() {
            cmd = cmd.arg(&format!("--cc={}", cc.trim()));
        }

        cmd.arg(patch_path)
    }

    /// Ask `git send-email --dry-run` to resolve the final visible To/Cc
    /// recipients for a patch. This deliberately lets Git perform its own
    /// author/trailer/ccCmd/alias logic instead of attempting to duplicate it.
    pub fn resolve_commit_email_recipients(
        &self,
        hash: &str,
        to: &str,
        cc: &str,
    ) -> Result<ResolvedEmailRecipients> {
        let (_temp_dir, patch_path) = self.format_commit_email_patch(hash)?;
        let result = self
            .send_email_command(&patch_path, to, cc, true, false)
            .run_expecting_success()?;

        parse_send_email_recipients(&result.stdout).with_context(
            || "git send-email --dry-run completed, but its final To/Cc headers could not be read",
        )
    }

    /// Format one commit into a temporary patch and send it with `git send-email`.
    ///
    /// SMTP/authentication/threading behavior remains owned by Git's existing
    /// `sendemail.*` configuration. Recipients have already been resolved by a
    /// dry-run review pass, so auto-CC is suppressed here and the visible Cc
    /// field is sent exactly as reviewed.
    pub fn send_commit_email(&self, hash: &str, to: &str, cc: &str, dry_run: bool) -> Result<()> {
        let (_temp_dir, patch_path) = self.format_commit_email_patch(hash)?;
        self.send_email_command(&patch_path, to, cc, dry_run, true)
            .run_expecting_success()?;
        Ok(())
    }
}

fn parse_send_email_recipients(output: &str) -> Option<ResolvedEmailRecipients> {
    let to = parse_send_email_header(output, "To").unwrap_or_default();
    let cc = parse_send_email_header(output, "Cc").unwrap_or_default();

    // A successful send-email invocation always has a To header unless all
    // addressing came from a mechanism Git does not print as a message header.
    // Treat a completely header-less log as unparseable rather than pretending
    // there are no recipients.
    if to.is_empty() && cc.is_empty() && !output.lines().any(|line| line.starts_with("From:")) {
        return None;
    }

    Some(ResolvedEmailRecipients { to, cc })
}

fn parse_send_email_header(output: &str, header: &str) -> Option<String> {
    let prefix = format!("{header}:");
    let mut in_log = false;
    let mut value: Option<String> = None;

    for line in output.lines() {
        if line.contains("Log says:") {
            in_log = true;
            continue;
        }
        if !in_log {
            continue;
        }

        if let Some(rest) = line.strip_prefix(&prefix) {
            value = Some(rest.trim().to_string());
            continue;
        }

        if let Some(current) = value.as_mut() {
            if line.starts_with(' ') || line.starts_with('\t') {
                let continuation = line.trim();
                if !continuation.is_empty() {
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(continuation);
                }
            } else {
                break;
            }
        }
    }

    value.map(|value| value.trim_end_matches(',').trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{TempPatchDir, parse_send_email_recipients};

    #[test]
    fn temp_patch_dir_is_cleaned_up() {
        let path = {
            let dir = TempPatchDir::create().unwrap();
            let path = dir.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists());
    }

    #[test]
    fn parses_resolved_send_email_recipients() {
        let output = r#"0001-test.patch
(mbox) Adding cc: A <author@example.com> from line 'From: A <author@example.com>'
Dry-OK. Log says:
Server: relay.example.com
MAIL FROM:<from@example.com>
RCPT TO:<to@example.com>
RCPT TO:<cc@example.com>
RCPT TO:<author@example.com>
From: Example <from@example.com>
To: to@example.com
Cc: cc@example.com,
 A <author@example.com>,
 reviewer@example.com
Subject: [PATCH] test
Date: DATE-STRING

Result: OK
"#;

        let recipients = parse_send_email_recipients(output).unwrap();
        assert_eq!(recipients.to, "to@example.com");
        assert_eq!(
            recipients.cc,
            "cc@example.com, A <author@example.com>, reviewer@example.com"
        );
    }

    #[test]
    fn parses_send_email_without_cc_header() {
        let output = r#"Dry-OK. Log says:
From: Example <from@example.com>
To: to@example.com
Subject: [PATCH] test

Result: OK
"#;

        let recipients = parse_send_email_recipients(output).unwrap();
        assert_eq!(recipients.to, "to@example.com");
        assert!(recipients.cc.is_empty());
    }
}
