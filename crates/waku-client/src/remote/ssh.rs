//! System OpenSSH invocation.
//!
//! Waku shells out to the user's `ssh` rather than embedding a client, so
//! `~/.ssh/config`, ssh-agent, `ProxyJump`, and known hosts all apply as they
//! do in a terminal.

use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};

use super::connection::{Connection, ConnectionKind, SshAuthMethod, SshTarget};
use super::process_util::hide_console_window;

const CONNECT_TIMEOUT_SECONDS: u64 = 5;
pub const ASKPASS_CONNECTION_ENV: &str = "WAKU_SSH_ASKPASS_CONNECTION_ID";
pub const ASKPASS_DIAGNOSTIC_PREFIX: &str = "[waku-ssh-askpass]";

/// Options shared by every ssh invocation. The destination is always the last
/// element, so callers can pop it to insert options before it.
pub fn base_args(target: &SshTarget) -> anyhow::Result<Vec<String>> {
    let password = matches!(target.auth_method(), SshAuthMethod::Password);
    let mut args = vec![
        "-T".to_owned(),
        "-o".to_owned(),
        format!("BatchMode={}", if password { "no" } else { "yes" }),
        "-o".to_owned(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
        // BatchMode turns ssh's host-key prompt into a hard failure, so trust
        // a new host on first use while still rejecting a changed key.
        "-o".to_owned(),
        "StrictHostKeyChecking=accept-new".to_owned(),
        // A RemoteCommand in the user's config would replace the bridge.
        "-o".to_owned(),
        "RemoteCommand=none".to_owned(),
        // Keep a quiet link alive and notice a dead one, so the desktop can
        // reconnect instead of waiting on a half-open TCP session.
        "-o".to_owned(),
        "ServerAliveInterval=15".to_owned(),
        "-o".to_owned(),
        "ServerAliveCountMax=3".to_owned(),
    ];
    if password {
        args.extend([
            "-o".to_owned(),
            "PreferredAuthentications=password,keyboard-interactive".to_owned(),
            "-o".to_owned(),
            "PubkeyAuthentication=no".to_owned(),
            "-o".to_owned(),
            "NumberOfPasswordPrompts=1".to_owned(),
        ]);
    }
    append_target_args(&mut args, target)?;
    Ok(args)
}

/// `ssh <options> <destination>` with askpass wiring for password auth. The
/// caller appends the remote command.
pub fn command(connection: &Connection) -> anyhow::Result<Command> {
    command_with_options(connection, &[])
}

/// `ssh` configured for streaming a binary upload, with `-C` compression so
/// large daemon binaries traverse slow or relayed links faster.
pub fn upload_command(connection: &Connection) -> anyhow::Result<Command> {
    command_with_options(connection, &["-C"])
}

fn command_with_options(
    connection: &Connection,
    extra_options: &[&str],
) -> anyhow::Result<Command> {
    let ConnectionKind::Ssh { target } = &connection.kind else {
        bail!("an SSH command requires an SSH connection");
    };
    let mut args = base_args(target)?;
    if !extra_options.is_empty() {
        let destination = args.pop().context("missing SSH destination")?;
        args.extend(extra_options.iter().map(|option| (*option).to_owned()));
        args.push(destination);
    }
    let mut command = Command::new("ssh");
    command.args(args);
    if matches!(target.auth_method(), SshAuthMethod::Password) {
        super::credentials::get_ssh_password(&connection.id)?;
        let executable = std::env::current_exe()
            .context("could not locate the Waku executable for SSH askpass")?;
        command.env("SSH_ASKPASS", executable);
        command.env("SSH_ASKPASS_REQUIRE", "force");
        command.env(ASKPASS_CONNECTION_ENV, &connection.id);
        if std::env::var_os("DISPLAY").is_none() {
            command.env("DISPLAY", "waku:0");
        }
        command.stdin(Stdio::null());
    }
    hide_console_window(&mut command);
    Ok(command)
}

fn append_target_args(args: &mut Vec<String>, target: &SshTarget) -> anyhow::Result<()> {
    match target {
        SshTarget::ConfigHost { host, .. } => {
            validate_destination(host, "SSH config host")?;
            args.push(host.trim().to_owned());
        }
        SshTarget::Direct {
            hostname,
            port,
            user,
            identity_file,
            auth_method,
            proxy_jump,
        } => {
            validate_destination(hostname, "SSH hostname")?;
            if *port == Some(0) {
                bail!("SSH port must be between 1 and 65535");
            }
            if let Some(port) = port {
                args.extend(["-p".to_owned(), port.to_string()]);
            }
            if let Some(user) = user.as_deref().filter(|user| !user.is_empty()) {
                validate_option_value(user, "SSH user")?;
                args.extend(["-l".to_owned(), user.to_owned()]);
            }
            let identity_file = identity_file
                .as_deref()
                .filter(|path| !path.trim().is_empty());
            if matches!(auth_method, SshAuthMethod::Key) && identity_file.is_none() {
                bail!("a private key path is required for key authentication");
            }
            if let Some(identity_file) = identity_file {
                if identity_file.contains('\0') {
                    bail!("SSH identity file is invalid");
                }
                args.extend(["-i".to_owned(), identity_file.to_owned()]);
                if matches!(auth_method, SshAuthMethod::Key) {
                    args.extend(["-o".to_owned(), "IdentitiesOnly=yes".to_owned()]);
                }
            }
            if let Some(proxy_jump) = proxy_jump.as_deref().filter(|jump| !jump.is_empty()) {
                validate_destination(proxy_jump, "SSH proxy jump")?;
                args.extend(["-J".to_owned(), proxy_jump.to_owned()]);
            }
            args.push(hostname.trim().to_owned());
        }
    }
    Ok(())
}

/// A destination starting with `-` would be parsed as an ssh option.
fn validate_destination(value: &str, label: &str) -> anyhow::Result<()> {
    let value = value.trim();
    if value.is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("{label} is invalid");
    }
    Ok(())
}

fn validate_option_value(value: &str, label: &str) -> anyhow::Result<()> {
    if value.trim().is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("{label} is invalid");
    }
    Ok(())
}

/// The remote side runs every command through `/bin/sh -c`, so values are
/// quoted once here and never interpolated raw.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn wrap_posix_script(script: &str) -> String {
    format!("/bin/sh -c {}", shell_quote(script))
}

/// Encode `script` as UTF-16LE Base64 for `powershell.exe -EncodedCommand`.
/// Base64 contains no metacharacters in `cmd.exe`, `powershell.exe`, `pwsh`,
/// or `/bin/sh`, so it runs identically under any Windows OpenSSH DefaultShell
/// while `-NoProfile` isolates it from broken user profiles and
/// `-OutputFormat Text` suppresses CLIXML progress serialization.
pub fn wrap_powershell_script(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "powershell.exe -NoProfile -NonInteractive -NoLogo -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand {}",
        encode_base64(&utf16le)
    )
}

/// Encode `script` to run directly inside an outer PowerShell `DefaultShell`
/// session (`pwsh.exe` or `powershell.exe`) so redirected `stdin` stays owned
/// by the script instead of being held by a parent PowerShell host.
pub fn wrap_powershell_in_process(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "& ([scriptblock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{}'))))",
        encode_base64(&utf16le)
    )
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let triple = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn strip_ansi_escapes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            if bytes.get(i + 1) == Some(&b'[') {
                let mut j = i + 2;
                while j < bytes.len() && (0x30..=0x3F).contains(&bytes[j]) {
                    j += 1;
                }
                while j < bytes.len() && (0x20..=0x2F).contains(&bytes[j]) {
                    j += 1;
                }
                if bytes.get(j).is_some_and(|b| (0x40..=0x7E).contains(b)) {
                    out.push_str(&text[last..i]);
                    i = j + 1;
                    last = i;
                    continue;
                }
                if j == bytes.len() {
                    out.push_str(&text[last..i]);
                    last = j;
                    break;
                }
            } else if bytes.get(i + 1) == Some(&b']') {
                let mut j = i + 2;
                let mut terminated = false;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        j += 1;
                        terminated = true;
                        break;
                    }
                    if bytes[j] == 0x1B && bytes.get(j + 1) == Some(&b'\\') {
                        j += 2;
                        terminated = true;
                        break;
                    }
                    j += 1;
                }
                if terminated || j == bytes.len() {
                    out.push_str(&text[last..i]);
                    i = j;
                    last = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&text[last..]);
    out
}

fn strip_powershell_clixml_progress(text: &str) -> String {
    let mut without_marker = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find("#< CLIXML") {
        let pos = cursor + offset;
        without_marker.push_str(&text[cursor..pos]);
        let mut next = pos + "#< CLIXML".len();
        if text[next..].starts_with("\r\n") {
            next += 2;
        } else if text[next..].starts_with('\n') {
            next += 1;
        }
        cursor = next;
    }
    without_marker.push_str(&text[cursor..]);

    let mut out = String::with_capacity(without_marker.len());
    let mut pos = 0;
    while let Some(start_rel) = without_marker[pos..].find("<Objs ") {
        let start = pos + start_rel;
        let (end, span, truncated) = match without_marker[start..].find("</Objs>") {
            Some(end_rel) => {
                let end = start + end_rel + "</Objs>".len();
                (end, &without_marker[start..end], false)
            }
            None => (without_marker.len(), &without_marker[start..], true),
        };
        let is_ps_clixml = span.contains("http://schemas.microsoft.com/powershell");
        if is_ps_clixml {
            out.push_str(&without_marker[pos..start]);
            let extracted = extract_clixml_diagnostics(span);
            if !extracted.is_empty() {
                out.push_str(&extracted);
                if !extracted.ends_with('\n') {
                    out.push('\n');
                }
            }
            let mut next = end;
            if !truncated && (start == 0 || without_marker[..start].ends_with('\n')) {
                if without_marker[next..].starts_with("\r\n") {
                    next += 2;
                } else if without_marker[next..].starts_with('\n') {
                    next += 1;
                }
            }
            pos = next;
        } else {
            out.push_str(&without_marker[pos..end]);
            pos = end;
        }
    }
    out.push_str(&without_marker[pos..]);
    out
}

fn extract_clixml_diagnostics(span: &str) -> String {
    let mut extracted = String::new();
    let mut cursor = 0;
    while let Some(tag_rel) = span[cursor..].find("<S S=\"") {
        let attr_start = cursor + tag_rel + "<S S=\"".len();
        let Some(quote_rel) = span[attr_start..].find('"') else {
            break;
        };
        let stream_name = &span[attr_start..attr_start + quote_rel];
        let after_quote = attr_start + quote_rel + 1;
        let Some(gt_rel) = span[after_quote..].find('>') else {
            break;
        };
        let content_start = after_quote + gt_rel + 1;
        let Some(close_rel) = span[content_start..].find("</S>") else {
            break;
        };
        let content_end = content_start + close_rel;
        if stream_name.eq_ignore_ascii_case("error") || stream_name.eq_ignore_ascii_case("warning")
        {
            let unescaped = decode_xml_entities(&span[content_start..content_end]);
            extracted.push_str(&decode_clixml_escapes(&unescaped));
        }
        cursor = content_end + "</S>".len();
    }
    extracted
}

fn decode_xml_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(amp_rel) = input[cursor..].find('&') {
        let amp = cursor + amp_rel;
        out.push_str(&input[cursor..amp]);
        let rest = &input[amp..];
        if let Some(after) = rest.strip_prefix("&lt;") {
            out.push('<');
            cursor = input.len() - after.len();
        } else if let Some(after) = rest.strip_prefix("&gt;") {
            out.push('>');
            cursor = input.len() - after.len();
        } else if let Some(after) = rest.strip_prefix("&quot;") {
            out.push('"');
            cursor = input.len() - after.len();
        } else if let Some(after) = rest.strip_prefix("&apos;") {
            out.push('\'');
            cursor = input.len() - after.len();
        } else if let Some(after) = rest.strip_prefix("&amp;") {
            out.push('&');
            cursor = input.len() - after.len();
        } else {
            out.push('&');
            cursor = amp + 1;
        }
    }
    out.push_str(&input[cursor..]);
    out
}

fn decode_clixml_escapes(input: &str) -> String {
    if !input.contains("_x") {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut pending_utf16: Vec<u16> = Vec::new();
    let flush_utf16 = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        for decoded in char::decode_utf16(pending.drain(..)) {
            out.push(decoded.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    };
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes.get(cursor..cursor + 2) == Some(b"_x")
            && bytes.get(cursor + 6) == Some(&b'_')
            && let Some(hex) = input.get(cursor + 2..cursor + 6)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(code_unit) = u16::from_str_radix(hex, 16)
        {
            pending_utf16.push(code_unit);
            cursor += 7;
            continue;
        }
        flush_utf16(&mut pending_utf16, &mut out);
        let ch = input[cursor..].chars().next().unwrap();
        out.push(ch);
        cursor += ch.len_utf8();
    }
    flush_utf16(&mut pending_utf16, &mut out);
    out
}

pub fn stderr_without_askpass_diagnostics(stderr: &[u8]) -> String {
    let filtered: String = String::from_utf8_lossy(stderr)
        .split_inclusive('\n')
        .filter(|line| !line.starts_with(ASKPASS_DIAGNOSTIC_PREFIX))
        .collect();
    strip_powershell_clixml_progress(&strip_ansi_escapes(&filtered))
}

/// Askpass entry point. When ssh launches Waku as `SSH_ASKPASS`, print the
/// stored password, flush stdout, and return an exit status so `main` exits
/// before any UI or daemon initialization. A failed lookup must exit non-zero
/// so OpenSSH aborts instead of submitting an empty password.
pub fn handle_askpass() -> Option<i32> {
    let connection_id = std::env::var_os(ASKPASS_CONNECTION_ENV)?;
    let connection_id = connection_id.to_string_lossy();
    match super::credentials::get_ssh_password(&connection_id) {
        Ok(password) => {
            let mut stdout = std::io::stdout();
            if let Err(error) = stdout
                .write_all(password.as_bytes())
                .and_then(|()| stdout.flush())
            {
                eprintln!("{ASKPASS_DIAGNOSTIC_PREFIX} {error:#}");
                return Some(1);
            }
            Some(0)
        }
        Err(error) => {
            eprintln!("{ASKPASS_DIAGNOSTIC_PREFIX} {error:#}");
            Some(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_pair(args: &[String], first: &str, second: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == first && pair[1] == second)
    }

    #[test]
    fn config_host_preserves_openssh_config() {
        let args = base_args(&SshTarget::ConfigHost {
            host: "devbox".into(),
            auth_method: SshAuthMethod::Agent,
        })
        .unwrap();
        assert_eq!(args.last().map(String::as_str), Some("devbox"));
        assert!(has_pair(&args, "-o", "BatchMode=yes"));
        assert!(has_pair(&args, "-o", "RemoteCommand=none"));
    }

    #[test]
    fn direct_target_keeps_explicit_options() {
        let args = base_args(&SshTarget::Direct {
            hostname: "192.0.2.10".into(),
            port: Some(2222),
            user: Some("deploy".into()),
            identity_file: Some("~/.ssh/deploy key".into()),
            auth_method: SshAuthMethod::Key,
            proxy_jump: Some("jump.example.com".into()),
        })
        .unwrap();
        assert!(has_pair(&args, "-p", "2222"));
        assert!(has_pair(&args, "-l", "deploy"));
        assert!(has_pair(&args, "-i", "~/.ssh/deploy key"));
        assert!(has_pair(&args, "-o", "IdentitiesOnly=yes"));
        assert!(has_pair(&args, "-J", "jump.example.com"));
        assert_eq!(args.last().map(String::as_str), Some("192.0.2.10"));
    }

    #[test]
    fn password_auth_enables_askpass_mode() {
        let args = base_args(&SshTarget::Direct {
            hostname: "example.com".into(),
            port: None,
            user: None,
            identity_file: None,
            auth_method: SshAuthMethod::Password,
            proxy_jump: None,
        })
        .unwrap();
        assert!(has_pair(&args, "-o", "BatchMode=no"));
        assert!(args.iter().any(|arg| arg == "PubkeyAuthentication=no"));
    }

    #[test]
    fn key_auth_requires_a_key() {
        assert!(
            base_args(&SshTarget::Direct {
                hostname: "example.com".into(),
                port: None,
                user: None,
                identity_file: None,
                auth_method: SshAuthMethod::Key,
                proxy_jump: None,
            })
            .is_err()
        );
    }

    #[test]
    fn rejects_option_like_or_whitespace_destinations() {
        for host in ["", "-Fother-config", "dev box", "dev\nbox"] {
            assert!(
                base_args(&SshTarget::ConfigHost {
                    host: host.into(),
                    auth_method: SshAuthMethod::Agent,
                })
                .is_err(),
                "{host:?}"
            );
        }
        assert!(
            base_args(&SshTarget::Direct {
                hostname: "example.com".into(),
                port: Some(0),
                user: None,
                identity_file: None,
                auth_method: SshAuthMethod::Agent,
                proxy_jump: None,
            })
            .is_err()
        );
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("/srv/it's waku"), "'/srv/it'\"'\"'s waku'");
    }

    #[test]
    fn askpass_diagnostics_are_filtered_and_ansi_stripped() {
        let stderr =
            b"[waku-ssh-askpass] missing\n\x1b[31;1mPermission\x1b[0m denied\n\x1b]0;title\x07ok\n";
        assert_eq!(
            stderr_without_askpass_diagnostics(stderr),
            "Permission denied\nok\n"
        );
    }

    #[test]
    fn base64_encodes_all_padding_lengths_and_alphabet() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(&[0xfb, 0xff, 0xbf]), "+/+/");
    }

    #[test]
    fn wrap_powershell_script_encodes_utf16le_with_noprofile_flags() {
        let wrapped = wrap_powershell_script("ok>?");
        assert_eq!(
            wrapped,
            "powershell.exe -NoProfile -NonInteractive -NoLogo -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand bwBrAD4APwA="
        );
    }

    #[test]
    fn wrap_powershell_in_process_avoids_quotes_and_uses_same_payload() {
        let wrapped = wrap_powershell_in_process("ok>?");
        assert_eq!(
            wrapped,
            "& ([scriptblock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('bwBrAD4APwA='))))"
        );
        assert!(!wrapped.contains('"'));
    }

    #[test]
    fn askpass_diagnostics_strip_clixml_progress_but_keep_errors() {
        let raw = concat!(
            "real failure\n",
            "#< CLIXML\r\n",
            "<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">",
            "<Obj S=\"progress\" RefId=\"0\"><PR N=\"Record\"><AV>Preparing</AV></PR></Obj>",
            "</Objs>\r\n",
            "<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">",
            "<Obj S=\"progress\" RefId=\"1\"><PR N=\"Record\"><AV>Preparing</AV></PR></Obj>",
            "<S S=\"error\">boom</S>",
            "</Objs>\n",
            "#< CLIXML\r\n",
            "<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">",
            "<S S=\"Error\">Access &lt;denied&gt; &amp; blocked._x000D__x000A_</S>",
            "</Objs>daemon_failed_to_run\tbad dll\n",
            "#< CLIXML\r\n",
            "<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">",
            "<Obj S=\"progress\" RefId=\"2\"><AV>truncated",
        );
        let cleaned = stderr_without_askpass_diagnostics(raw.as_bytes());
        assert!(cleaned.contains("real failure\n"), "{cleaned:?}");
        assert!(!cleaned.contains("#< CLIXML"), "{cleaned:?}");
        assert!(!cleaned.contains("<Objs"), "{cleaned:?}");
        assert!(!cleaned.contains("Preparing"), "{cleaned:?}");
        assert!(cleaned.contains("boom\n"), "{cleaned:?}");
        assert!(
            cleaned.contains("Access <denied> & blocked.\r\n"),
            "{cleaned:?}"
        );
        assert!(
            cleaned.contains("daemon_failed_to_run\tbad dll\n"),
            "{cleaned:?}"
        );
    }

    #[test]
    fn clixml_literal_underscore_x_escape_and_surrogates_decode_cleanly() {
        assert_eq!(decode_clixml_escapes("_x005F_x000A_"), "_x000A_");
        assert_eq!(decode_clixml_escapes("_xD83D__xDE00_"), "😀");
    }
}
