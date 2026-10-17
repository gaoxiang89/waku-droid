//! `wsl.exe` invocation and distribution discovery.

use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

use anyhow::bail;

use super::process_util::{hide_console_window, output_with_timeout, with_detail};

const WSL_PROGRAM: &str = "wsl.exe";
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Installed distributions, in `wsl.exe` order. Blocking: call it off the UI
/// thread.
pub fn list_distributions() -> anyhow::Result<Vec<String>> {
    if !cfg!(windows) {
        bail!("WSL is only available on Windows");
    }
    let mut command = Command::new(WSL_PROGRAM);
    command.args(["--list", "--quiet"]);
    let output = output_with_timeout(command, LIST_TIMEOUT, "wsl.exe --list")?;
    if !output.status.success() {
        bail!(
            "{}",
            with_detail(
                &format!("wsl.exe --list exited with {}", output.status),
                decode_wsl_text(&output.stderr).trim(),
            )
        );
    }
    Ok(parse_distribution_list(&output.stdout))
}

/// Run a POSIX script in `distro`. `--exec` bypasses the user's login shell so
/// rc-file output cannot corrupt a protocol stream on stdout.
pub fn command(distro: &str, script: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new(WSL_PROGRAM);
    command.args(["--distribution", distro, "--exec", "/bin/sh", "-c", script]);
    command.args(arguments);
    hide_console_window(&mut command);
    command
}

fn parse_distribution_list(bytes: &[u8]) -> Vec<String> {
    let text = decode_wsl_text(bytes);
    let mut seen = HashSet::new();
    text.lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}').trim())
        .filter(|line| !line.is_empty())
        .filter(|line| seen.insert((*line).to_owned()))
        .map(str::to_owned)
        .collect()
}

/// `wsl.exe` writes its own messages as UTF-16LE, with or without a BOM,
/// while commands run inside a distribution write UTF-8.
pub fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let looks_utf16_le =
        bytes.len() >= 2 && (bytes.starts_with(&[0xff, 0xfe]) || bytes.contains(&0));
    if looks_utf16_le {
        let offset = usize::from(bytes.starts_with(&[0xff, 0xfe])) * 2;
        let units = bytes[offset..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&units)
            .trim_matches('\0')
            .to_owned();
    }
    String::from_utf8_lossy(bytes)
        .trim_start_matches('\u{feff}')
        .trim_matches('\0')
        .to_owned()
}

/// `wslpath` and many loaders reject Win32 verbatim prefixes.
pub fn normalize_windows_path(value: &str) -> String {
    if let Some(value) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{value}");
    }
    value.strip_prefix(r"\\?\").unwrap_or(value).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utf16le_distribution_output() {
        let bytes = "\u{feff}Debian\r\nUbuntu-24.04\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(parse_distribution_list(&bytes), ["Debian", "Ubuntu-24.04"]);
    }

    #[test]
    fn parses_utf8_output_and_deduplicates() {
        assert_eq!(
            parse_distribution_list(b"Debian\n\nDebian\nUbuntu\n"),
            ["Debian", "Ubuntu"]
        );
    }

    #[test]
    fn strips_verbatim_prefixes() {
        assert_eq!(
            normalize_windows_path(r"\\?\C:\Program Files\Waku\waku-daemon"),
            r"C:\Program Files\Waku\waku-daemon"
        );
        assert_eq!(
            normalize_windows_path(r"\\?\UNC\server\share\waku"),
            r"\\server\share\waku"
        );
    }
}
