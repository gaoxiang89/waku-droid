//! System OpenSSH invocation.
//!
//! Waku shells out to the user's `ssh` rather than embedding a client, so
//! `~/.ssh/config`, ssh-agent, `ProxyJump`, and known hosts all apply as they
//! do in a terminal.

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
    let ConnectionKind::Ssh { target } = &connection.kind else {
        bail!("an SSH command requires an SSH connection");
    };
    let mut command = Command::new("ssh");
    command.args(base_args(target)?);
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

pub fn stderr_without_askpass_diagnostics(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .split_inclusive('\n')
        .filter(|line| !line.starts_with(ASKPASS_DIAGNOSTIC_PREFIX))
        .collect()
}

/// Askpass entry point. When ssh launches Waku as `SSH_ASKPASS`, print the
/// stored password and report that the process should exit before any UI or
/// daemon initialization. Must run first in `main`.
pub fn handle_askpass() -> bool {
    let Some(connection_id) = std::env::var_os(ASKPASS_CONNECTION_ENV) else {
        return false;
    };
    let connection_id = connection_id.to_string_lossy();
    match super::credentials::get_ssh_password(&connection_id) {
        Ok(password) => print!("{password}"),
        Err(error) => eprintln!("{ASKPASS_DIAGNOSTIC_PREFIX} {error:#}"),
    }
    true
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
    fn askpass_diagnostics_are_filtered() {
        let stderr = b"[waku-ssh-askpass] missing\nPermission denied\n";
        assert_eq!(
            stderr_without_askpass_diagnostics(stderr),
            "Permission denied\n"
        );
    }
}
