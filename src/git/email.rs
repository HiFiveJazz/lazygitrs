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
    /// Sender shown by `git send-email --dry-run`.
    pub preview_from: String,
    /// Final subject shown by `git send-email --dry-run`.
    pub preview_subject: String,
    /// Date header shown by `git send-email --dry-run` (or format-patch fallback).
    pub preview_date: String,
    /// Message body/diff produced by format-patch, adjusted for a differing sender.
    pub preview_body: String,
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
        let patch = fs::read_to_string(&patch_path)
            .with_context(|| format!("Failed to read generated patch {patch_path}"))?;
        let result = self
            .send_email_command(&patch_path, to, cc, true, false)
            .run_expecting_success()?;

        let mut resolved = parse_send_email_recipients(&result.stdout).with_context(
            || "git send-email --dry-run completed, but its final To/Cc headers could not be read",
        )?;
        populate_send_email_preview(&mut resolved, &result.stdout, &patch);
        Ok(resolved)
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

    Some(ResolvedEmailRecipients {
        to,
        cc,
        preview_from: String::new(),
        preview_subject: String::new(),
        preview_date: String::new(),
        preview_body: String::new(),
    })
}

fn populate_send_email_preview(
    resolved: &mut ResolvedEmailRecipients,
    send_email_output: &str,
    patch: &str,
) {
    let patch_from = parse_format_patch_header(patch, "From").unwrap_or_default();
    let patch_subject = parse_format_patch_header(patch, "Subject").unwrap_or_default();
    let patch_date = parse_format_patch_header(patch, "Date").unwrap_or_default();

    resolved.preview_from = parse_send_email_header(send_email_output, "From")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| patch_from.clone());
    resolved.preview_subject = parse_send_email_header(send_email_output, "Subject")
        .filter(|value| !value.is_empty())
        .unwrap_or(patch_subject);
    resolved.preview_date = parse_send_email_header(send_email_output, "Date")
        .filter(|value| !value.is_empty())
        .unwrap_or(patch_date);

    let mut body = format_patch_body(patch);
    if !patch_from.is_empty()
        && mailbox_key(&patch_from) != mailbox_key(&resolved.preview_from)
        && !body.starts_with("From: ")
    {
        body = format!("From: {patch_from}\n\n{body}");
    }
    resolved.preview_body = body;
}

fn parse_format_patch_header(patch: &str, header: &str) -> Option<String> {
    let normalized = patch.replace("\r\n", "\n");
    let prefix = format!("{header}:");
    let mut value: Option<String> = None;

    for (index, line) in normalized.lines().enumerate() {
        if index == 0 && line.starts_with("From ") {
            continue;
        }
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix(&prefix) {
            value = Some(rest.trim().to_string());
            continue;
        }
        if let Some(current) = value.as_mut() {
            if line.starts_with(' ') || line.starts_with('\t') {
                let continuation = line.trim();
                if !continuation.is_empty() {
                    current.push(' ');
                    current.push_str(continuation);
                }
            } else {
                break;
            }
        }
    }

    value.map(|value| decode_rfc2047_words(value.trim()))
}

fn format_patch_body(patch: &str) -> String {
    let normalized = patch.replace("\r\n", "\n");
    let message = normalized
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or(&normalized);
    message
        .split_once("\n\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}

fn mailbox_key(value: &str) -> String {
    let value = value.trim();
    if let (Some(open), Some(close)) = (value.rfind('<'), value.rfind('>')) {
        if open < close {
            return value[open + 1..close].trim().to_ascii_lowercase();
        }
    }
    value.to_ascii_lowercase()
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

    value.map(|value| decode_rfc2047_words(value.trim_end_matches(',').trim()))
}

/// Decode RFC 2047 encoded-words used in mail header display names.
///
/// `git send-email --dry-run` prints the final RFC-formatted To/Cc headers,
/// so non-ASCII names can arrive as e.g.
/// `=?UTF-8?q?J=C3=A9r=C3=A9mie=20Rodon?=`.  Decode those before exposing
/// recipients to the TUI so the compose/review UI always works with the
/// human-readable form. Unsupported/malformed encoded-words are preserved.
fn decode_rfc2047_words(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("=?") {
        out.push_str(&rest[..start]);
        let encoded = &rest[start..];

        let Some(charset_end_rel) = encoded[2..].find('?') else {
            out.push_str(encoded);
            return out;
        };
        let charset_end = 2 + charset_end_rel;
        let charset = &encoded[2..charset_end];
        let after_charset = charset_end + 1;

        let Some(encoding_end_rel) = encoded[after_charset..].find('?') else {
            out.push_str(encoded);
            return out;
        };
        let encoding_end = after_charset + encoding_end_rel;
        let encoding = &encoded[after_charset..encoding_end];
        let text_start = encoding_end + 1;

        let Some(text_end_rel) = encoded[text_start..].find("?=") else {
            out.push_str(encoded);
            return out;
        };
        let text_end = text_start + text_end_rel;
        let encoded_text = &encoded[text_start..text_end];
        let word_end = text_end + 2;

        let bytes = if encoding.eq_ignore_ascii_case("q") {
            decode_rfc2047_q(encoded_text)
        } else if encoding.eq_ignore_ascii_case("b") {
            decode_rfc2047_base64(encoded_text)
        } else {
            None
        };

        let decoded = bytes.and_then(|bytes| decode_rfc2047_charset(charset, &bytes));
        if let Some(decoded) = decoded {
            out.push_str(&decoded);
        } else {
            out.push_str(&encoded[..word_end]);
        }

        rest = &encoded[word_end..];
    }

    out.push_str(rest);
    out
}

fn decode_rfc2047_q(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_nibble(bytes[i + 1])?;
                let lo = hex_nibble(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }

    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_rfc2047_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut quartet = [0u8; 4];
    let mut len = 0usize;
    let mut padding = 0usize;

    for byte in input.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }

        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                0
            }
            _ => return None,
        };

        if padding > 0 && byte != b'=' {
            return None;
        }

        quartet[len] = value;
        len += 1;

        if len == 4 {
            out.push((quartet[0] << 2) | (quartet[1] >> 4));
            if padding < 2 {
                out.push((quartet[1] << 4) | (quartet[2] >> 2));
            }
            if padding == 0 {
                out.push((quartet[2] << 6) | quartet[3]);
            }
            len = 0;
            padding = 0;
        }
    }

    if len != 0 {
        return None;
    }

    Some(out)
}

fn decode_rfc2047_charset(charset: &str, bytes: &[u8]) -> Option<String> {
    if charset.eq_ignore_ascii_case("utf-8") || charset.eq_ignore_ascii_case("utf8") {
        String::from_utf8(bytes.to_vec()).ok()
    } else if charset.eq_ignore_ascii_case("us-ascii") || charset.eq_ignore_ascii_case("ascii") {
        bytes
            .is_ascii()
            .then(|| String::from_utf8_lossy(bytes).into_owned())
    } else if charset.eq_ignore_ascii_case("iso-8859-1") || charset.eq_ignore_ascii_case("latin1") {
        Some(bytes.iter().map(|byte| char::from(*byte)).collect())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TempPatchDir, decode_rfc2047_words, parse_send_email_recipients,
        populate_send_email_preview,
    };

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
    fn builds_preview_with_sender_and_original_author() {
        let output = r#"Dry-OK. Log says:
From: Jazz <jazz@example.com>
To: to@example.com
Cc: reviewer@example.com
Subject: [PATCH] io_uring: test
Date: Thu, 30 Oct 2026 10:00:00 -0700

Result: OK
"#;
        let patch = r#"From deadbeef Mon Sep 17 00:00:00 2001
From: Jens Axboe <axboe@kernel.dk>
Date: Thu, 30 Oct 2026 09:00:00 -0700
Subject: [PATCH] io_uring: test

Patch body

---
 file | 1 +
"#;
        let mut resolved = parse_send_email_recipients(output).unwrap();
        populate_send_email_preview(&mut resolved, output, patch);
        assert_eq!(resolved.preview_from, "Jazz <jazz@example.com>");
        assert_eq!(resolved.preview_subject, "[PATCH] io_uring: test");
        assert!(resolved.preview_body.starts_with("From: Jens Axboe <axboe@kernel.dk>\n\nPatch body"));
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
    #[test]
    fn decodes_rfc2047_q_encoded_recipient_names() {
        assert_eq!(
            decode_rfc2047_words("=?UTF-8?q?J=C3=A9r=C3=A9mie=20Rodon?= <jeremie.rodon@gmail.com>"),
            "Jérémie Rodon <jeremie.rodon@gmail.com>"
        );
        assert_eq!(
            decode_rfc2047_words(
                "=?UTF-8?q?Christoph=20B=C3=B6hmwalder?= <christoph.boehmwalder@linbit.com>"
            ),
            "Christoph Böhmwalder <christoph.boehmwalder@linbit.com>"
        );
    }

    #[test]
    fn decodes_rfc2047_base64_encoded_recipient_name() {
        assert_eq!(
            decode_rfc2047_words("=?UTF-8?B?SsOpcsOpbWllIFJvZG9u?= <jeremie@example.com>"),
            "Jérémie Rodon <jeremie@example.com>"
        );
    }

    #[test]
    fn parses_decoded_rfc2047_send_email_recipients() {
        let output = r#"Dry-OK. Log says:
From: Example <from@example.com>
To: to@example.com
Cc: =?UTF-8?q?J=C3=A9r=C3=A9mie=20Rodon?= <jeremie.rodon@gmail.com>,
 =?UTF-8?q?Christoph=20B=C3=B6hmwalder?= <christoph.boehmwalder@linbit.com>
Subject: [PATCH] test

Result: OK
"#;

        let recipients = parse_send_email_recipients(output).unwrap();
        assert_eq!(
            recipients.cc,
            "Jérémie Rodon <jeremie.rodon@gmail.com>, Christoph Böhmwalder <christoph.boehmwalder@linbit.com>"
        );
    }
}
