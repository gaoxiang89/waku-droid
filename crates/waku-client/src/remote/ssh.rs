//! System OpenSSH invocation.
//!
//! Waku shells out to the user's `ssh` rather than embedding a client, so
//! `~/.ssh/config`, ssh-agent, `ProxyJump`, and known hosts all apply as they
//! do in a terminal.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};

use super::connection::{Connection, ConnectionKind, SshAuthMethod, SshTarget};
use super::process_util::hide_console_window;

const CONNECT_TIMEOUT_SECONDS: u64 = 5;
pub const ASKPASS_CONNECTION_ENV: &str = "WAKU_SSH_ASKPASS_CONNECTION_ID";
pub const ASKPASS_DIAGNOSTIC_PREFIX: &str = "[waku-ssh-askpass]";
const WINDOWS_AUTH_HINT: &str =
    "Hint: if your key uses a passphrase, save it in Connections or start the Windows ssh-agent service.";

pub fn resolve_ssh_program() -> PathBuf {
    resolve_ssh_program_with(
        std::env::var_os("WAKU_SSH_BINARY").as_deref(),
        std::env::var_os("SystemRoot").as_deref(),
    )
}

fn resolve_ssh_program_with(
    override_binary: Option<&OsStr>,
    system_root: Option<&OsStr>,
) -> PathBuf {
    if let Some(binary) = override_binary.filter(|value| !value.is_empty()) {
        return PathBuf::from(binary);
    }
    #[cfg(windows)]
    if let Some(system_root) = system_root.filter(|value| !value.is_empty()) {
        let candidate = PathBuf::from(system_root)
            .join("System32")
            .join("OpenSSH")
            .join("ssh.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    #[cfg(not(windows))]
    let _ = system_root;
    PathBuf::from("ssh")
}

/// Options shared by every ssh invocation. The destination is always the last
/// element, so callers can pop it to insert options before it.
pub fn base_args(target: &SshTarget) -> anyhow::Result<Vec<String>> {
    let password = matches!(target.auth_method(), SshAuthMethod::Password);
    let has_proxy_jump = matches!(
        target,
        SshTarget::Direct {
            proxy_jump: Some(jump),
            ..
        } if !jump.trim().is_empty()
    );
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
        if !has_proxy_jump {
            args.extend([
                "-o".to_owned(),
                "PreferredAuthentications=password,keyboard-interactive".to_owned(),
                "-o".to_owned(),
                "PubkeyAuthentication=no".to_owned(),
            ]);
        }
        args.extend(["-o".to_owned(), "NumberOfPasswordPrompts=1".to_owned()]);
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
    let use_askpass = match target.auth_method() {
        SshAuthMethod::Password => {
            super::credentials::get_ssh_password(&connection.id)?;
            true
        }
        SshAuthMethod::Key => super::credentials::has_ssh_password(&connection.id),
        SshAuthMethod::Agent => false,
    };
    build_command_with_options(connection, extra_options, use_askpass)
}

fn build_command_with_options(
    connection: &Connection,
    extra_options: &[&str],
    use_askpass: bool,
) -> anyhow::Result<Command> {
    let ConnectionKind::Ssh { target } = &connection.kind else {
        bail!("an SSH command requires an SSH connection");
    };
    let mut args = base_args(target)?;
    let key_passphrase_askpass =
        use_askpass && matches!(target.auth_method(), SshAuthMethod::Key);
    if key_passphrase_askpass {
        for arg in &mut args {
            if arg == "BatchMode=yes" {
                *arg = "BatchMode=no".to_owned();
            }
        }
    }
    if key_passphrase_askpass || !extra_options.is_empty() {
        let destination = args.pop().context("missing SSH destination")?;
        if key_passphrase_askpass {
            args.extend(["-o".to_owned(), "NumberOfPasswordPrompts=1".to_owned()]);
        }
        args.extend(extra_options.iter().map(|option| (*option).to_owned()));
        args.push(destination);
    }
    let mut command = Command::new(resolve_ssh_program());
    command.args(args);
    if let Some(path) = crate::command_env::executable_search_path() {
        command.env("PATH", path);
    }
    if use_askpass {
        let executable = std::env::current_exe()
            .context("could not locate the Waku executable for SSH askpass")?;
        command.env("SSH_ASKPASS", executable);
        command.env("SSH_ASKPASS_REQUIRE", "force");
        command.env(ASKPASS_CONNECTION_ENV, &connection.id);
        if std::env::var_os("DISPLAY").is_none_or(|v| v.is_empty()) {
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
            if let Some(user) = user.as_deref().map(str::trim).filter(|user| !user.is_empty()) {
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
            if let Some(proxy_jump) = proxy_jump
                .as_deref()
                .map(str::trim)
                .filter(|jump| !jump.is_empty())
            {
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
    let encoded = encode_powershell_utf16le(script);
    format!(
        "powershell.exe -NoProfile -NonInteractive -NoLogo -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand {encoded}"
    )
}

/// Fallback for `cmd.exe` `DefaultShell` hosts whose `PATH` omits
/// `%SystemRoot%\System32\WindowsPowerShell\v1.0`.
pub fn wrap_cmd_systemroot_powershell_script(script: &str) -> String {
    let encoded = encode_powershell_utf16le(script);
    format!(
        "\"%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -NoProfile -NonInteractive -NoLogo -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand {encoded}"
    )
}

/// Fallback for Git Bash / MSYS2 / Cygwin `DefaultShell` hosts on Windows,
/// resolving `powershell.exe` from `SYSTEMROOT` when non-login `PATH` omits it.
pub fn wrap_posix_powershell_script(script: &str) -> String {
    let encoded = encode_powershell_utf16le(script);
    let posix = format!(
        "ps=\"powershell.exe\"; command -v \"$ps\" >/dev/null 2>&1 || ps=\"${{SYSTEMROOT:-C:/Windows}}/System32/WindowsPowerShell/v1.0/powershell.exe\"; exec \"$ps\" -NoProfile -NonInteractive -NoLogo -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand {encoded}"
    );
    wrap_posix_script(&posix)
}

/// Encode `script` to run directly inside an outer PowerShell `DefaultShell`
/// session (`pwsh.exe` or `powershell.exe`) so redirected `stdin` stays owned
/// by the script instead of being held by a parent PowerShell host.
pub fn wrap_powershell_in_process(script: &str) -> String {
    let encoded = encode_powershell_utf16le(script);
    format!(
        "$ProgressPreference='SilentlyContinue'; & ([scriptblock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{encoded}'))))"
    )
}

fn encode_powershell_utf16le(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    encode_base64(&utf16le)
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

pub fn append_auth_hint(connection: &Connection, detail: String) -> String {
    if !cfg!(windows) {
        return detail;
    }
    let ConnectionKind::Ssh { target } = &connection.kind else {
        return detail;
    };
    let auth_method = target.auth_method();
    if !detail.contains("Permission denied")
        || matches!(auth_method, SshAuthMethod::Password)
        || detail.contains(WINDOWS_AUTH_HINT)
    {
        return detail;
    }
    let has_passphrase = matches!(auth_method, SshAuthMethod::Key)
        && super::credentials::has_ssh_password(&connection.id);
    append_windows_auth_hint_if_needed(detail, auth_method, has_passphrase, true)
}

fn append_windows_auth_hint_if_needed(
    detail: String,
    auth_method: SshAuthMethod,
    has_passphrase: bool,
    is_windows: bool,
) -> String {
    if !is_windows
        || !detail.contains("Permission denied")
        || detail.contains(WINDOWS_AUTH_HINT)
    {
        return detail;
    }
    let missing_secret = match auth_method {
        SshAuthMethod::Agent => true,
        SshAuthMethod::Key => !has_passphrase,
        SshAuthMethod::Password => false,
    };
    if !missing_secret {
        return detail;
    }
    format!("{}\n{WINDOWS_AUTH_HINT}", detail.trim_end())
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
            "$ProgressPreference='SilentlyContinue'; & ([scriptblock]::Create([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('bwBrAD4APwA='))))"
        );
        assert!(!wrapped.contains('"'));
    }

    #[test]
    fn wrap_powershell_fallbacks_preserve_encoded_payload() {
        let cmd_wrapped = wrap_cmd_systemroot_powershell_script("ok>?");
        assert!(cmd_wrapped.starts_with(
            "\"%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -NoProfile "
        ));
        assert!(cmd_wrapped.ends_with(" -EncodedCommand bwBrAD4APwA="));

        let posix_wrapped = wrap_posix_powershell_script("ok>?");
        assert!(posix_wrapped.starts_with("/bin/sh -c '"));
        assert!(posix_wrapped.contains("WindowsPowerShell/v1.0/powershell.exe"));
        assert!(posix_wrapped.contains("-EncodedCommand bwBrAD4APwA="));
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

    #[test]
    fn password_auth_with_proxy_jump_keeps_jump_pubkey_auth_enabled() {
        let args = base_args(&SshTarget::Direct {
            hostname: "target.internal".into(),
            port: None,
            user: Some("deploy".into()),
            identity_file: None,
            auth_method: SshAuthMethod::Password,
            proxy_jump: Some("bastion.example.com".into()),
        })
        .unwrap();
        assert!(has_pair(&args, "-o", "BatchMode=no"));
        assert!(has_pair(&args, "-o", "NumberOfPasswordPrompts=1"));
        assert!(has_pair(&args, "-J", "bastion.example.com"));
        assert!(!args.iter().any(|arg| arg == "PubkeyAuthentication=no"));
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("PreferredAuthentications="))
        );

        let whitespace_jump_args = base_args(&SshTarget::Direct {
            hostname: "target.internal".into(),
            port: None,
            user: None,
            identity_file: None,
            auth_method: SshAuthMethod::Password,
            proxy_jump: Some("   ".into()),
        })
        .unwrap();
        assert!(
            whitespace_jump_args
                .iter()
                .any(|arg| arg == "PubkeyAuthentication=no")
        );
        assert!(
            whitespace_jump_args
                .iter()
                .any(|arg| arg == "PreferredAuthentications=password,keyboard-interactive")
        );
        assert!(!whitespace_jump_args.iter().any(|arg| arg == "-J"));
    }

    #[test]
    fn option_trimming_handles_user_proxy_jump_and_leading_dash() {
        let args = base_args(&SshTarget::Direct {
            hostname: "  example.com  ".into(),
            port: None,
            user: Some("  deploy  ".into()),
            identity_file: None,
            auth_method: SshAuthMethod::Agent,
            proxy_jump: Some("  jump.example.com  ".into()),
        })
        .unwrap();
        assert!(has_pair(&args, "-l", "deploy"));
        assert!(has_pair(&args, "-J", "jump.example.com"));
        assert_eq!(args.last().map(String::as_str), Some("example.com"));

        assert!(validate_option_value(" -oProxyCommand=bad ", "SSH user").is_err());
        assert!(validate_option_value("  alice  ", "SSH user").is_ok());
    }

    #[test]
    fn key_passphrase_askpass_wiring_rewrites_batch_mode_and_envs() {
        let connection = Connection {
            id: "ssh:key-passphrase-test".into(),
            name: "SSH · test".into(),
            kind: ConnectionKind::Ssh {
                target: SshTarget::Direct {
                    hostname: "example.com".into(),
                    port: None,
                    user: Some("alice".into()),
                    identity_file: Some("C:/Users/alice/.ssh/id_ed25519".into()),
                    auth_method: SshAuthMethod::Key,
                    proxy_jump: None,
                },
            },
        };

        let wired = build_command_with_options(&connection, &["-C"], true).unwrap();
        let wired_args: Vec<String> = wired
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(has_pair(&wired_args, "-o", "BatchMode=no"));
        assert!(!wired_args.iter().any(|arg| arg == "BatchMode=yes"));
        assert!(has_pair(&wired_args, "-o", "NumberOfPasswordPrompts=1"));
        assert!(has_pair(&wired_args, "-o", "IdentitiesOnly=yes"));
        assert!(wired_args.iter().any(|arg| arg == "-C"));
        assert_eq!(wired_args.last().map(String::as_str), Some("example.com"));

        let wired_envs: Vec<(String, Option<String>)> = wired
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|val| val.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            wired_envs
                .iter()
                .any(|(k, v)| k == "SSH_ASKPASS" && v.as_deref().is_some_and(|p| !p.is_empty()))
        );
        assert!(
            wired_envs
                .iter()
                .any(|(k, v)| k == "SSH_ASKPASS_REQUIRE" && v.as_deref() == Some("force"))
        );
        assert!(
            wired_envs.iter().any(|(k, v)| k == ASKPASS_CONNECTION_ENV
                && v.as_deref() == Some("ssh:key-passphrase-test"))
        );
        assert!(wired_envs.iter().any(|(k, _)| k == "PATH"));

        let plain = build_command_with_options(&connection, &[], false).unwrap();
        let plain_args: Vec<String> = plain
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(has_pair(&plain_args, "-o", "BatchMode=yes"));
        assert!(
            !plain_args
                .iter()
                .any(|arg| arg == "NumberOfPasswordPrompts=1")
        );
        assert!(
            !plain
                .get_envs()
                .any(|(k, _)| k == OsStr::new("SSH_ASKPASS"))
        );
    }

    #[test]
    fn resolve_ssh_program_prefers_override_then_windows_openssh() {
        assert_eq!(
            resolve_ssh_program_with(
                Some(OsStr::new("/custom/ssh")),
                Some(OsStr::new("C:\\Windows"))
            ),
            PathBuf::from("/custom/ssh")
        );
        assert_eq!(
            resolve_ssh_program_with(Some(OsStr::new("")), None),
            PathBuf::from("ssh")
        );

        #[cfg(windows)]
        {
            let root = std::env::temp_dir().join(format!("waku-ssh-bin-{}", uuid::Uuid::new_v4()));
            let openssh_dir = root.join("System32").join("OpenSSH");
            std::fs::create_dir_all(&openssh_dir).unwrap();
            let ssh_exe = openssh_dir.join("ssh.exe");
            std::fs::write(&ssh_exe, b"").unwrap();
            assert_eq!(
                resolve_ssh_program_with(None, Some(root.as_os_str())),
                ssh_exe
            );
            let _ = std::fs::remove_dir_all(&root);

            let empty_root =
                std::env::temp_dir().join(format!("waku-ssh-empty-{}", uuid::Uuid::new_v4()));
            assert_eq!(
                resolve_ssh_program_with(None, Some(empty_root.as_os_str())),
                PathBuf::from("ssh")
            );
        }
    }

    #[test]
    fn windows_auth_hint_applies_only_to_agent_or_key_without_passphrase() {
        let denied = "alice@host: Permission denied (publickey).".to_owned();
        let with_agent =
            append_windows_auth_hint_if_needed(denied.clone(), SshAuthMethod::Agent, false, true);
        assert!(with_agent.contains(WINDOWS_AUTH_HINT));

        let key_no_pass =
            append_windows_auth_hint_if_needed(denied.clone(), SshAuthMethod::Key, false, true);
        assert!(key_no_pass.contains(WINDOWS_AUTH_HINT));

        let key_with_pass =
            append_windows_auth_hint_if_needed(denied.clone(), SshAuthMethod::Key, true, true);
        assert_eq!(key_with_pass, denied);

        let with_password =
            append_windows_auth_hint_if_needed(denied.clone(), SshAuthMethod::Password, false, true);
        assert_eq!(with_password, denied);

        let non_windows =
            append_windows_auth_hint_if_needed(denied.clone(), SshAuthMethod::Agent, false, false);
        assert_eq!(non_windows, denied);
    }
}
