//! Sessionless Claude Code initialization metadata.
//!
//! Claude's stream-json transport exposes the same account- and
//! configuration-aware command and model catalogs that back its interactive
//! pickers. The probe sends only the SDK initialization control request, so it
//! neither runs a model turn nor persists a conversation.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const REQUEST_ID: &str = "waku-initialize-catalog";

/// Return Claude Code's initialization response for the requested settings
/// scope. A project-aware command probe supplies its cwd and all setting
/// sources; the global model probe intentionally reads only user settings.
pub(crate) fn initialize(
    binary: &Path,
    cwd: Option<&Path>,
    config_dir: Option<&Path>,
    setting_sources: &str,
) -> Option<Value> {
    let request = json!({
        "type": "control_request",
        "request_id": REQUEST_ID,
        "request": {"subtype": "initialize"}
    });
    let mut input = serde_json::to_vec(&request).ok()?;
    input.push(b'\n');

    let mut command = crate::command_env::command(binary);
    crate::command_env::apply_provider_config_dir(
        &mut command,
        crate::model::ProviderKind::Claude,
        config_dir,
    );
    command
        .args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--no-session-persistence",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--strict-mcp-config",
            "--setting-sources",
            setting_sources,
        ])
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    isolate_process_group(&mut command);
    let mut child = crate::command_env::spawn(&mut command).ok()?;
    #[cfg(windows)]
    let job = crate::process_tree::KillOnCloseJob::adopt(child.id()).ok();
    let Some(mut stdin) = child.stdin.take() else {
        terminate_child(
            &mut child,
            #[cfg(windows)]
            job,
        );
        return None;
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_child(
            &mut child,
            #[cfg(windows)]
            job,
        );
        return None;
    };
    let (tx, rx) = mpsc::channel();
    let Ok(reader) = std::thread::Builder::new()
        .name("waku-claude-metadata".into())
        .spawn(move || {
            stream_bounded_json_lines(BufReader::new(stdout), MAX_LINE_BYTES, &tx);
        })
    else {
        terminate_child(
            &mut child,
            #[cfg(windows)]
            job,
        );
        return None;
    };

    let response = if stdin.write_all(&input).is_ok() && stdin.flush().is_ok() {
        drop(stdin);
        let deadline = Instant::now() + PROBE_TIMEOUT;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break None;
            };
            let Ok(value) = rx.recv_timeout(remaining) else {
                break None;
            };
            if is_initialize_response(&value) {
                break Some(value);
            }
        }
    } else {
        None
    };
    terminate_child(
        &mut child,
        #[cfg(windows)]
        job,
    );
    let _ = reader.join();
    response
}

fn is_initialize_response(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("control_response")
        && value
            .pointer("/response/request_id")
            .and_then(Value::as_str)
            == Some(REQUEST_ID)
}

#[derive(Debug, PartialEq, Eq)]
enum BoundedLine {
    Eof,
    Oversized,
    Line(String),
}

/// Read one newline-delimited record while capping buffered allocation at
/// `max_bytes`. Any line exceeding `max_bytes` is drained to its newline
/// without retaining its payload so subsequent lines can still be parsed.
fn read_bounded_line(reader: &mut impl BufRead, max_bytes: usize) -> std::io::Result<BoundedLine> {
    let mut bytes = Vec::new();
    let mut saw_any = false;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        saw_any = true;
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if !oversized {
                let slice = &available[..newline];
                let slice = slice.strip_suffix(b"\r").unwrap_or(slice);
                if bytes.len().saturating_add(slice.len()) > max_bytes {
                    oversized = true;
                    bytes.clear();
                } else {
                    bytes.extend_from_slice(slice);
                }
            }
            reader.consume(newline + 1);
            break;
        }
        let len = available.len();
        if !oversized {
            if bytes.len().saturating_add(len) > max_bytes {
                // Allow a single trailing `\r` before `\n` at the boundary.
                let trailing_cr = available.last() == Some(&b'\r')
                    && bytes.len().saturating_add(len - 1) <= max_bytes;
                if trailing_cr {
                    bytes.extend_from_slice(&available[..len - 1]);
                } else {
                    oversized = true;
                    bytes.clear();
                }
            } else {
                bytes.extend_from_slice(available);
            }
        }
        reader.consume(len);
    }
    if !saw_any {
        return Ok(BoundedLine::Eof);
    }
    if oversized {
        return Ok(BoundedLine::Oversized);
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    match String::from_utf8(bytes) {
        Ok(line) => Ok(BoundedLine::Line(line)),
        Err(_) => Ok(BoundedLine::Oversized),
    }
}

fn stream_bounded_json_lines(
    mut reader: impl BufRead,
    max_line_bytes: usize,
    tx: &mpsc::Sender<Value>,
) {
    loop {
        match read_bounded_line(&mut reader, max_line_bytes) {
            Ok(BoundedLine::Eof) | Err(_) => break,
            Ok(BoundedLine::Oversized) => continue,
            Ok(BoundedLine::Line(line)) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        }
    }
}

fn isolate_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

fn terminate_child(
    child: &mut Child,
    #[cfg(windows)] job: Option<crate::process_tree::KillOnCloseJob>,
) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id())
        && pid > 0
    {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    drop(job);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_line_reader_skips_oversized_records_without_dropping_following_responses() {
        let oversized = format!(r#"{{"type":"noise","pad":"{}"}}"#, "x".repeat(512));
        let valid = json!({
            "type": "control_response",
            "response": {
                "request_id": REQUEST_ID,
                "subtype": "success",
                "response": {"commands": []}
            }
        });
        let input = format!("{oversized}\nnot-json\n{valid}\n");

        let (tx, rx) = mpsc::channel();
        stream_bounded_json_lines(Cursor::new(input), 256, &tx);
        drop(tx);

        let received: Vec<_> = rx.into_iter().collect();
        assert_eq!(received.len(), 1);
        assert!(is_initialize_response(&received[0]));
    }

    #[test]
    fn read_bounded_line_enforces_exact_cap_and_strips_crlf() {
        let input = b"12345\r\n123456\r\nok\n";
        let mut reader = Cursor::new(&input[..]);
        assert_eq!(
            read_bounded_line(&mut reader, 5).unwrap(),
            BoundedLine::Line("12345".into())
        );
        assert_eq!(
            read_bounded_line(&mut reader, 5).unwrap(),
            BoundedLine::Oversized
        );
        assert_eq!(
            read_bounded_line(&mut reader, 5).unwrap(),
            BoundedLine::Line("ok".into())
        );
        assert_eq!(read_bounded_line(&mut reader, 5).unwrap(), BoundedLine::Eof);
    }

    #[test]
    fn terminate_child_kills_descendants_so_inherited_stdout_reaches_eof() {
        use std::io::Read as _;

        #[cfg(windows)]
        let mut command = {
            let mut cmd = crate::command_env::plain_command("cmd.exe");
            cmd.args(["/d", "/c", "ping -n 60 127.0.0.1"]);
            cmd
        };
        #[cfg(unix)]
        let mut command = {
            let mut cmd = crate::command_env::plain_command("/bin/sh");
            cmd.args(["-c", "sleep 60"]);
            cmd
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        isolate_process_group(&mut command);

        let mut child = crate::command_env::spawn(&mut command).expect("spawn shell grandchild");
        #[cfg(windows)]
        let job = crate::process_tree::KillOnCloseJob::adopt(child.id()).ok();
        let mut stdout = child.stdout.take().expect("stdout pipe");
        // Give the wrapper shell time to launch its grandchild inside the job / group.
        std::thread::sleep(Duration::from_millis(500));

        let (done_tx, done_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut drained = Vec::new();
            let _ = stdout.read_to_end(&mut drained);
            let _ = done_tx.send(());
        });

        terminate_child(
            &mut child,
            #[cfg(windows)]
            job,
        );

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("grandchild kept stdout open after terminate_child");
        reader.join().unwrap();
    }
}
