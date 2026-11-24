//! Provider-owned slash-command discovery.
//!
//! The composer must not guess a CLI's built-ins. Providers with a safe,
//! sessionless catalog surface are probed here; providers whose command list
//! is session-scoped publish it through their live driver instead. Every probe
//! runs in the daemon's background workspace operation, never on a render path.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use waku_protocol::composer::{CommandScope, SlashCommand};
use waku_protocol::model::ProviderKind;

const CLI_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const COMMAND_CATALOG_CAP: usize = 500;

/// Discover the command surface the installed CLI can expose without creating
/// a provider session. `None` means either that the provider only reports
/// commands from a real session or that its probe failed; filesystem-defined
/// commands remain available as the caller's fallback in both cases.
pub(crate) fn discover(
    provider: ProviderKind,
    binary: &Path,
    project_root: &Path,
    config_dir: Option<&Path>,
) -> Option<Vec<SlashCommand>> {
    match provider {
        ProviderKind::Amp => discover_amp(binary, project_root),
        ProviderKind::Antigravity => discover_antigravity(binary, project_root, config_dir),
        ProviderKind::Claude => discover_claude(binary, project_root, config_dir),
        ProviderKind::Codex => discover_codex(binary, project_root, config_dir),
        ProviderKind::Droid => Some(droid_bridged_cli_commands()),
        ProviderKind::OpenCode => discover_opencode(binary, project_root),
        ProviderKind::OhMyPi => discover_oh_my_pi(binary, project_root),
        ProviderKind::Pi => discover_pi(binary, project_root),
        // ACP advertises commands only after session/new. Harness likewise
        // requires an agent id for commands/list. Creating throwaway sessions
        // merely to seed autocomplete would pollute provider history, so their
        // live DriverEvent::AvailableCommands update is the catalog surface.
        ProviderKind::Cursor
        | ProviderKind::DeepSeek
        | ProviderKind::Fx
        | ProviderKind::Grok
        | ProviderKind::Kimi => None,
    }
}

fn discover_antigravity(
    binary: &Path,
    project_root: &Path,
    config_dir: Option<&Path>,
) -> Option<Vec<SlashCommand>> {
    discover_antigravity_with(
        config_dir,
        || crate::model_catalog::discover_antigravity_acp_commands_in(binary, config_dir),
        || discover_antigravity_skills(binary, project_root, config_dir),
    )
}

fn discover_antigravity_with(
    config_dir: Option<&Path>,
    probe_acp: impl FnOnce() -> Option<Vec<SlashCommand>>,
    probe_cli_skills: impl FnOnce() -> Option<Vec<SlashCommand>>,
) -> Option<Vec<SlashCommand>> {
    let acp_commands = probe_acp();
    // Root `agy -p "/skills"` lacks `--gemini-dir` and reads the default profile's
    // user skills; isolated accounts rely on `composer_complete` directory scans.
    let skills = if config_dir.is_none() {
        probe_cli_skills()
    } else {
        None
    };
    let bridged = antigravity_bridged_cli_commands();
    let merged = merge_antigravity_catalog(
        acp_commands.unwrap_or_default(),
        skills.unwrap_or_default(),
        bridged,
    );
    (!merged.is_empty()).then_some(merged)
}

fn discover_antigravity_skills(
    binary: &Path,
    project_root: &Path,
    config_dir: Option<&Path>,
) -> Option<Vec<SlashCommand>> {
    let value = capture_json_in(
        binary,
        &["-p", "/skills", "--output-format", "json"],
        project_root,
        Some(ProviderKind::Antigravity),
        config_dir,
    )?;
    Some(parse_antigravity_skills(&value))
}

fn parse_antigravity_skills(value: &Value) -> Vec<SlashCommand> {
    value
        .pointer("/command/data/skills")
        .or_else(|| value.get("skills"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|skill| {
            command(
                skill.get("name")?.as_str()?,
                skill.get("description").and_then(Value::as_str),
                CommandScope::Skill,
                None,
                None,
            )
        })
        .take(COMMAND_CATALOG_CAP)
        .collect()
}

/// CLI-local Antigravity commands that Waku bridges in the composer without
/// starting a provider turn. Kept 1:1 with `waku-client/composer_complete.rs`.
fn antigravity_bridged_cli_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "usage".to_owned(),
            description: crate::i18n::translate("commands.usage_description"),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: None,
        },
        SlashCommand {
            name: "quota".to_owned(),
            description: crate::i18n::translate("commands.quota_description"),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: None,
        },
        SlashCommand {
            name: "credits".to_owned(),
            description: crate::i18n::translate("commands.credits_description"),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: None,
        },
        SlashCommand {
            name: "model".to_owned(),
            description: crate::i18n::translate("commands.model_description"),
            scope: CommandScope::Builtin,
            argument_hint: Some("[<model>]".to_owned()),
            template: None,
        },
        SlashCommand {
            name: "effort".to_owned(),
            description: crate::i18n::translate("commands.effort_description"),
            scope: CommandScope::Builtin,
            argument_hint: Some("[low|medium|high|xhigh]".to_owned()),
            template: None,
        },
    ]
}

/// CLI-local Factory Droid commands that Waku bridges in the composer without
/// starting an ACP turn or spawning a throwaway discovery session. Kept 1:1
/// with `waku-client/composer_complete.rs`.
fn droid_bridged_cli_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "model".to_owned(),
            description: crate::i18n::translate("commands.model_description"),
            scope: CommandScope::Builtin,
            argument_hint: Some("[model]".to_owned()),
            template: None,
        },
        SlashCommand {
            name: "reasoning".to_owned(),
            description: crate::i18n::translate("commands.effort_description"),
            scope: CommandScope::Builtin,
            argument_hint: Some("[level]".to_owned()),
            template: None,
        },
        SlashCommand {
            name: "effort".to_owned(),
            description: crate::i18n::translate("commands.effort_description"),
            scope: CommandScope::Builtin,
            argument_hint: Some("[level]".to_owned()),
            template: None,
        },
        SlashCommand {
            name: "autonomy".to_owned(),
            description: crate::i18n::translate("commands.autonomy_description"),
            scope: CommandScope::Builtin,
            argument_hint: Some("[normal|auto-low|auto-medium|auto-high]".to_owned()),
            template: None,
        },
        SlashCommand {
            name: "new".to_owned(),
            description: crate::i18n::translate("commands.new_session_description"),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: None,
        },
        SlashCommand {
            name: "clear".to_owned(),
            description: crate::i18n::translate("commands.new_session_description"),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: None,
        },
    ]
}

fn antigravity_default_argument_hint(name: &str) -> Option<&'static str> {
    match name {
        "swarm" => Some("<task> | board"),
        "boost" => Some("<task>"),
        "plan" => Some("[<task>]"),
        "goal" => Some("<objective>"),
        "compact" => Some("[focus]"),
        "schedule" => Some("<spec>"),
        "browser" => Some("<task>"),
        "teamwork" => Some("<task>"),
        "run-workflow" => Some("<workflow>"),
        _ => None,
    }
}

/// Merges Antigravity's ACP `available_commands_update` built-ins, zero-quota
/// `/skills` listing, and Waku-bridged CLI-local commands without letting a
/// skill entry downgrade an ACP built-in (e.g. `swarm`, `run-workflow`) or
/// strip its argument hint.
fn merge_antigravity_catalog(
    acp_commands: Vec<SlashCommand>,
    skills: Vec<SlashCommand>,
    bridged_cli: Vec<SlashCommand>,
) -> Vec<SlashCommand> {
    let mut commands = Vec::new();
    let mut seen_builtins = HashSet::new();

    for mut entry in acp_commands {
        if entry.argument_hint.is_none()
            && let Some(hint) = antigravity_default_argument_hint(&entry.name)
        {
            entry.argument_hint = Some(hint.to_owned());
        }
        if seen_builtins.insert(entry.name.clone()) {
            commands.push(entry);
        }
    }
    for entry in bridged_cli {
        if let Some(existing) = commands
            .iter_mut()
            .find(|existing| existing.name == entry.name)
        {
            if existing.description.is_empty() && !entry.description.is_empty() {
                existing.description = entry.description;
            }
            if existing.argument_hint.is_none() && entry.argument_hint.is_some() {
                existing.argument_hint = entry.argument_hint;
            }
        } else if seen_builtins.insert(entry.name.clone()) {
            commands.push(entry);
        }
    }
    for skill in skills {
        if let Some(existing) = commands
            .iter_mut()
            .find(|existing| existing.name == skill.name)
        {
            if existing.description.is_empty() && !skill.description.is_empty() {
                existing.description = skill.description;
            }
        } else {
            commands.push(skill);
        }
    }
    commands.truncate(COMMAND_CATALOG_CAP);
    commands
}

fn discover_amp(binary: &Path, project_root: &Path) -> Option<Vec<SlashCommand>> {
    let value = capture_json(binary, &["skill", "list", "--json"], project_root)?;
    Some(parse_amp_skills(&value))
}

fn discover_claude(
    binary: &Path,
    project_root: &Path,
    config_dir: Option<&Path>,
) -> Option<Vec<SlashCommand>> {
    // This is the initialization request used by the Claude Agent SDK. No user
    // message follows it, so the CLI loads local commands without making a
    // model request or persisting a conversation.
    let value = crate::claude_metadata::initialize(
        binary,
        Some(project_root),
        config_dir,
        "user,project,local",
    )?;
    Some(parse_claude_commands(&value))
}

fn discover_codex(
    binary: &Path,
    project_root: &Path,
    config_dir: Option<&Path>,
) -> Option<Vec<SlashCommand>> {
    let mut command = crate::command_env::command(binary);
    crate::command_env::apply_provider_config_dir(&mut command, ProviderKind::Codex, config_dir);
    command
        .args(["app-server", "--stdio"])
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    isolate_process_group(&mut command);
    let mut child = crate::command_env::spawn(&mut command).ok()?;
    let Some(mut stdin) = child.stdin.take() else {
        terminate_child(&mut child);
        return None;
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return None;
    };
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || read_json_lines(stdout, tx));

    let initialize = json!({
        "method": "initialize",
        "id": 0,
        "params": {
            "clientInfo": {
                "name": "waku",
                "title": "Waku",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {"experimentalApi": true}
        }
    });
    let commands = if write_json_line(&mut stdin, &initialize).is_ok()
        && recv_rpc_response(&rx, 0, CLI_PROBE_TIMEOUT).is_some()
        && write_json_line(&mut stdin, &json!({"method": "initialized", "params": {}})).is_ok()
    {
        let mut commands = Vec::new();
        // Codex does not publish a TUI command registry, but it does publish
        // the effective feature set backing the native commands Waku bridges.
        // Availability therefore comes from the installed CLI/config rather
        // than from an unconditional Codex list in the composer.
        let feature_request = json!({
            "method": "experimentalFeature/list",
            "id": 1,
            "params": {"cursor": null, "limit": COMMAND_CATALOG_CAP, "threadId": null}
        });
        if write_json_line(&mut stdin, &feature_request).is_ok()
            && let Some(value) = recv_rpc_response(&rx, 1, CLI_PROBE_TIMEOUT)
        {
            commands.extend(parse_codex_command_features(&value));
        }

        let cwd = project_root.to_string_lossy();
        let request = json!({
            "method": "skills/list",
            "id": 2,
            "params": {"cwds": [cwd.as_ref()]}
        });
        if write_json_line(&mut stdin, &request).is_ok() {
            if let Some(value) = recv_rpc_response(&rx, 2, CLI_PROBE_TIMEOUT) {
                commands.extend(parse_codex_skills(&value, project_root));
            }
        }
        Some(commands)
    } else {
        None
    };
    terminate_child(&mut child);
    drop(stdin);
    let _ = reader.join();
    commands
}

fn discover_opencode(binary: &Path, project_root: &Path) -> Option<Vec<SlashCommand>> {
    // The registry is directory-scoped, not session-scoped. Seed the new-task
    // composer only when an OpenCode service is already running, never
    // starting the user's background daemon or creating a disposable session.
    discover_opencode_with(
        project_root,
        || {
            crate::opencode_service::attached(binary)
                .ok()
                .flatten()
                .map(|service| service.endpoint())
        },
        |endpoint, directory| crate::opencode_api::list_commands(endpoint, Some(directory)).ok(),
    )
}

fn discover_opencode_with(
    project_root: &Path,
    attach: impl FnOnce() -> Option<crate::http_wire::Endpoint>,
    list_commands: impl FnOnce(
        &crate::http_wire::Endpoint,
        &str,
    ) -> Option<Vec<crate::opencode_api::CommandInfo>>,
) -> Option<Vec<SlashCommand>> {
    let directory = crate::opencode_api::canonical_directory(project_root).ok()?;
    let endpoint = attach()?;
    let commands = list_commands(&endpoint, &directory)?;
    Some(opencode_commands(commands))
}

fn discover_pi(binary: &Path, project_root: &Path) -> Option<Vec<SlashCommand>> {
    // Catalog reads neither run an agent turn nor need the runtime driver's
    // full-access flag.
    let request = json!({"id": "waku-command-catalog", "type": "get_commands"});
    let input = format!("{request}\n");
    let value = probe_json_lines(
        binary,
        &["--mode", "rpc", "--no-session", "--offline"],
        project_root,
        input.as_bytes(),
        CLI_PROBE_TIMEOUT,
        |value| {
            value.get("id").and_then(Value::as_str) == Some("waku-command-catalog")
                && value.get("success").and_then(Value::as_bool) == Some(true)
        },
        &[("PI_SKIP_VERSION_CHECK", "1")],
    )?;
    Some(parse_pi_commands(&value))
}

fn discover_oh_my_pi(binary: &Path, project_root: &Path) -> Option<Vec<SlashCommand>> {
    // OMP publishes its full registry at RPC startup; no request or provider
    // session is needed. A blank frame lets stdin close cleanly on older builds.
    let value = probe_json_lines(
        binary,
        &["--mode", "rpc", "--no-session"],
        project_root,
        b"\n",
        CLI_PROBE_TIMEOUT,
        |value| value.get("type").and_then(Value::as_str) == Some("available_commands_update"),
        &[],
    )?;
    Some(parse_oh_my_pi_commands(&value))
}

fn parse_amp_skills(value: &Value) -> Vec<SlashCommand> {
    value
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|skill| {
            command(
                skill.get("name")?.as_str()?,
                skill.get("description").and_then(Value::as_str),
                CommandScope::Skill,
                None,
                None,
            )
        })
        .take(COMMAND_CATALOG_CAP)
        .collect()
}

fn parse_claude_commands(value: &Value) -> Vec<SlashCommand> {
    value
        .pointer("/response/response/commands")
        .or_else(|| value.get("commands"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            command(
                entry.get("name")?.as_str()?,
                entry.get("description").and_then(Value::as_str),
                CommandScope::Builtin,
                entry.get("argumentHint").and_then(Value::as_str),
                None,
            )
        })
        .take(COMMAND_CATALOG_CAP)
        .collect()
}

fn parse_codex_command_features(value: &Value) -> Vec<SlashCommand> {
    value
        .pointer("/result/data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|feature| feature.get("enabled").and_then(Value::as_bool) == Some(true))
        .filter_map(
            |feature| match feature.get("name").and_then(Value::as_str)? {
                "fast_mode" => Some(SlashCommand {
                    name: "fast".to_owned(),
                    description: crate::i18n::translate("commands.fast_description"),
                    scope: CommandScope::Builtin,
                    argument_hint: None,
                    template: None,
                }),
                "goals" => Some(SlashCommand {
                    name: "goal".to_owned(),
                    description: crate::i18n::translate("commands.goal_description"),
                    scope: CommandScope::Builtin,
                    argument_hint: Some("[<objective>|clear|edit|pause|resume]".to_owned()),
                    template: None,
                }),
                _ => None,
            },
        )
        .collect()
}

fn parse_codex_skills(value: &Value, project_root: &Path) -> Vec<SlashCommand> {
    let data = value.pointer("/result/data").and_then(Value::as_array);
    let cwd = project_root.to_string_lossy();
    let entries = data.into_iter().flatten();
    let matching = entries
        .clone()
        .find(|entry| entry.get("cwd").and_then(Value::as_str) == Some(cwd.as_ref()));
    let skills = if let Some(entry) = matching {
        entry
            .get("skills")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    } else {
        entries
            .flat_map(|entry| {
                entry
                    .get("skills")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .collect()
    };
    skills
        .into_iter()
        .filter(|skill| skill.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|skill| {
            let description = skill
                .get("shortDescription")
                .or_else(|| skill.pointer("/interface/shortDescription"))
                .or_else(|| skill.get("description"))
                .and_then(Value::as_str);
            command(
                skill.get("name")?.as_str()?,
                description,
                CommandScope::Skill,
                None,
                None,
            )
        })
        .take(COMMAND_CATALOG_CAP)
        .collect()
}

/// The service's command registry: built-ins, configured commands and MCP
/// prompts, which the server expands itself. Skills are not commands here;
/// OpenCode attaches them to a prompt as `@` mentions.
fn opencode_commands(commands: Vec<crate::opencode_api::CommandInfo>) -> Vec<SlashCommand> {
    commands
        .into_iter()
        .filter_map(|entry| {
            command(
                &entry.name,
                entry.description.as_deref(),
                CommandScope::Builtin,
                None,
                None,
            )
        })
        .take(COMMAND_CATALOG_CAP)
        .collect()
}

pub(crate) fn parse_pi_commands(value: &Value) -> Vec<SlashCommand> {
    value
        .pointer("/data/commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let source = entry.get("source").and_then(Value::as_str);
            let raw_name = entry.get("name")?.as_str()?;
            let name = source
                .filter(|source| *source == "skill")
                .and_then(|_| raw_name.strip_prefix("skill:"))
                .unwrap_or(raw_name);
            let scope = match source {
                Some("skill") => CommandScope::Skill,
                Some("prompt") => {
                    match entry.pointer("/sourceInfo/scope").and_then(Value::as_str) {
                        Some("project") => CommandScope::Project,
                        _ => CommandScope::User,
                    }
                }
                _ => CommandScope::Builtin,
            };
            command(
                name,
                entry.get("description").and_then(Value::as_str),
                scope,
                entry.pointer("/input/hint").and_then(Value::as_str),
                None,
            )
        })
        .take(COMMAND_CATALOG_CAP)
        .collect()
}

pub(crate) fn parse_oh_my_pi_commands(value: &Value) -> Vec<SlashCommand> {
    value
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let source = entry.get("source").and_then(Value::as_str);
            let raw_name = entry.get("name")?.as_str()?;
            let name = source
                .filter(|source| *source == "skill")
                .and_then(|_| raw_name.strip_prefix("skill:"))
                .unwrap_or(raw_name);
            command(
                name,
                entry.get("description").and_then(Value::as_str),
                if source == Some("skill") {
                    CommandScope::Skill
                } else {
                    CommandScope::Builtin
                },
                entry.pointer("/input/hint").and_then(Value::as_str),
                None,
            )
        })
        .take(COMMAND_CATALOG_CAP)
        .collect()
}

fn command(
    raw_name: &str,
    description: Option<&str>,
    scope: CommandScope,
    argument_hint: Option<&str>,
    template: Option<&str>,
) -> Option<SlashCommand> {
    let name = raw_name.trim().trim_start_matches('/');
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return None;
    }
    Some(SlashCommand {
        name: name.to_owned(),
        description: description.unwrap_or_default().trim().to_owned(),
        scope,
        argument_hint: argument_hint
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_owned),
        template: template
            .map(str::trim)
            .filter(|template| !template.is_empty())
            .map(str::to_owned),
    })
}

fn build_capture_command(
    binary: &Path,
    args: &[&str],
    cwd: &Path,
    provider: Option<ProviderKind>,
    config_dir: Option<&Path>,
) -> Command {
    let mut command = crate::command_env::command(binary);
    if let Some(provider) = provider {
        crate::command_env::apply_provider_config_dir(&mut command, provider, config_dir);
        if provider == ProviderKind::Antigravity
            && let Some(settings_path) = crate::command_env::antigravity_settings_arg(config_dir)
        {
            command.arg("--settings").arg(settings_path);
        }
    }
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn capture_json(binary: &Path, args: &[&str], cwd: &Path) -> Option<Value> {
    capture_json_in(binary, args, cwd, None, None)
}

fn capture_json_in(
    binary: &Path,
    args: &[&str],
    cwd: &Path,
    provider: Option<ProviderKind>,
    config_dir: Option<&Path>,
) -> Option<Value> {
    let mut command = build_capture_command(binary, args, cwd, provider, config_dir);
    isolate_process_group(&mut command);
    let mut child = crate::command_env::spawn(&mut command).ok()?;
    #[cfg(windows)]
    let job = crate::process_tree::KillOnCloseJob::adopt(child.id()).ok();
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return None;
    };
    let reader = thread::spawn(move || read_bounded(stdout, MAX_CAPTURE_BYTES));
    let success = wait_for_child(&mut child, CLI_PROBE_TIMEOUT);
    #[cfg(windows)]
    drop(job);
    let output = reader.join().ok().flatten()?;
    success
        .then(|| serde_json::from_slice(&output).ok())
        .flatten()
}

#[allow(clippy::too_many_arguments)]
fn probe_json_lines(
    binary: &Path,
    args: &[&str],
    cwd: &Path,
    input: &[u8],
    timeout: Duration,
    matches: impl Fn(&Value) -> bool,
    env: &[(&str, &str)],
) -> Option<Value> {
    let mut command = crate::command_env::command(binary);
    command
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    isolate_process_group(&mut command);
    let mut child = crate::command_env::spawn(&mut command).ok()?;
    let Some(mut stdin) = child.stdin.take() else {
        terminate_child(&mut child);
        return None;
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return None;
    };
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || read_json_lines(stdout, tx));
    let result = if stdin.write_all(input).is_ok() && stdin.flush().is_ok() {
        drop(stdin);
        recv_matching(&rx, timeout, matches)
    } else {
        None
    };
    terminate_child(&mut child);
    let _ = reader.join();
    result
}

fn read_json_lines(reader: impl Read, tx: mpsc::Sender<Value>) {
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        if line.len() > MAX_CAPTURE_BYTES {
            continue;
        }
        if let Ok(value) = serde_json::from_str(&line)
            && tx.send(value).is_err()
        {
            break;
        }
    }
}

fn recv_matching(
    rx: &Receiver<Value>,
    timeout: Duration,
    matches: impl Fn(&Value) -> bool,
) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let value = rx.recv_timeout(remaining).ok()?;
        if matches(&value) {
            return Some(value);
        }
    }
}

fn recv_rpc_response(rx: &Receiver<Value>, id: u64, timeout: Duration) -> Option<Value> {
    recv_matching(rx, timeout, |value| {
        value.get("id").and_then(Value::as_u64) == Some(id)
    })
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&chunk[..read.min(remaining)]);
        exceeded |= read > remaining;
    }
    (!exceeded).then_some(output)
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                terminate_child(child);
                return false;
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

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_initialization_metadata() {
        let commands = parse_claude_commands(&json!({
            "type": "control_response",
            "response": {
                "request_id": "waku-command-catalog",
                "response": {"commands": [
                    {"name": "compact", "description": "Compact context", "argumentHint": "[focus]"},
                    {"name": "  ", "description": "ignored"}
                ]}
            }
        }));
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "compact");
        assert_eq!(commands[0].description, "Compact context");
        assert_eq!(commands[0].argument_hint.as_deref(), Some("[focus]"));
        assert_eq!(commands[0].scope, CommandScope::Builtin);
    }

    #[test]
    fn parses_codex_enabled_skills_for_the_requested_cwd() {
        let root = Path::new("/repo");
        let commands = parse_codex_skills(
            &json!({"result": {"data": [
                {"cwd": "/other", "skills": [{"name": "other", "enabled": true, "description": "Other"}]},
                {"cwd": "/repo", "skills": [
                    {"name": "review", "enabled": true, "description": "Long", "shortDescription": "Review changes"},
                    {"name": "off", "enabled": false, "description": "Disabled"}
                ]}
            ]}}),
            root,
        );
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].description, "Review changes");
        assert_eq!(commands[0].scope, CommandScope::Skill);
    }

    #[test]
    fn derives_codex_native_commands_from_enabled_cli_features() {
        let commands = parse_codex_command_features(&json!({"result": {"data": [
            {"name": "fast_mode", "enabled": true},
            {"name": "goals", "enabled": true},
            {"name": "unrelated", "enabled": true}
        ]}}));
        let fast = commands
            .iter()
            .find(|command| command.name == "fast")
            .expect("enabled fast_mode must expose /fast");
        assert_eq!(fast.scope, CommandScope::Builtin);
        assert!(fast.template.is_none());
        let goal = commands
            .iter()
            .find(|command| command.name == "goal")
            .expect("enabled goals must expose /goal");
        assert_eq!(goal.scope, CommandScope::Builtin);
        assert!(goal.template.is_none());
        assert!(goal.argument_hint.is_some());

        let disabled = parse_codex_command_features(&json!({"result": {"data": [
            {"name": "fast_mode", "enabled": false},
            {"name": "goals", "enabled": false}
        ]}}));
        assert!(disabled.is_empty());
    }

    #[test]
    fn parses_pi_and_oh_my_pi_native_shapes() {
        let pi = parse_pi_commands(&json!({"data": {"commands": [
            {"name": "deploy", "description": "Deploy", "source": "prompt", "sourceInfo": {"scope": "project"}},
            {"name": "skill:verify", "description": "Verify", "source": "skill"},
            {"name": "websearch", "description": "Search", "source": "extension"}
        ]}}));
        assert_eq!(pi[0].scope, CommandScope::Project);
        assert_eq!(pi[1].name, "verify");
        assert_eq!(pi[1].scope, CommandScope::Skill);
        assert_eq!(pi[2].scope, CommandScope::Builtin);

        let omp = parse_oh_my_pi_commands(&json!({
            "type": "available_commands_update",
            "commands": [
                {"name": "compact", "description": "Compact", "source": "builtin", "input": {"hint": "[focus]"}},
                {"name": "skill:verify", "description": "Verify", "source": "skill"}
            ]
        }));
        assert_eq!(omp[0].argument_hint.as_deref(), Some("[focus]"));
        assert_eq!(omp[1].name, "verify");
        assert_eq!(omp[1].scope, CommandScope::Skill);
    }

    #[test]
    fn opencode_registry_commands_are_server_expanded_builtins() {
        let commands = opencode_commands(
            serde_json::from_value(json!([
                {"name": "init", "description": "guided AGENTS.md setup"},
                {"name": "review", "description": "review changes [commit|branch|pr], defaults to uncommitted"},
                {"name": "mcp:lookup"},
                {"name": "invalid command"}
            ]))
            .unwrap(),
        );
        assert_eq!(
            commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["init", "review", "mcp:lookup"]
        );
        assert!(
            commands
                .iter()
                .all(|command| command.scope == CommandScope::Builtin)
        );
        assert!(commands.iter().all(|command| command.template.is_none()));
    }

    #[test]
    fn discover_opencode_returns_none_without_running_service_and_normalizes_directory() {
        let root =
            std::env::temp_dir().join(format!("waku-slash-opencode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let absent = discover_opencode_with(
            &root,
            || None,
            |_, _| unreachable!("must not list commands when no service is attached"),
        );
        assert!(absent.is_none());

        let mut captured_directory = String::new();
        let present = discover_opencode_with(
            &root,
            || Some(crate::http_wire::Endpoint::local(1)),
            |_, directory| {
                captured_directory = directory.to_owned();
                Some(vec![crate::opencode_api::CommandInfo {
                    name: "review".to_owned(),
                    description: Some("Review changes".to_owned()),
                }])
            },
        )
        .expect("attached service should return commands");
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].name, "review");
        assert!(
            !captured_directory.starts_with(r"\\?\"),
            "Windows verbatim prefix must be stripped: {captured_directory}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parses_amp_skills() {
        let amp = parse_amp_skills(&json!({"skills": [
            {"name": "building-skills", "description": "Build skills", "source": "builtin"}
        ]}));
        assert_eq!(amp.len(), 1);
        assert_eq!(amp[0].scope, CommandScope::Skill);
    }

    #[test]
    fn parses_antigravity_skills_and_merges_without_downgrading_acp_builtins() {
        let skills = parse_antigravity_skills(&json!({
            "conversation_id": "",
            "status": "SUCCESS",
            "num_turns": 0,
            "command": {
                "name": "skills",
                "data": {
                    "skills": [
                        {"name": "swarm", "description": "Skill swarm description", "builtin": true, "model_invocable": false},
                        {"name": "antigravity-guide", "description": "Guide to Antigravity", "builtin": true, "model_invocable": true},
                        {"name": "bad skill", "description": "Ignored whitespace"}
                    ]
                }
            }
        }));
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "swarm");
        assert_eq!(skills[0].scope, CommandScope::Skill);
        assert_eq!(skills[1].name, "antigravity-guide");
        assert_eq!(skills[1].scope, CommandScope::Skill);

        let acp = vec![
            SlashCommand {
                name: "boost".into(),
                description: "Boost output".into(),
                scope: CommandScope::Builtin,
                argument_hint: Some("<prompt>".into()),
                template: None,
            },
            SlashCommand {
                name: "swarm".into(),
                description: String::new(),
                scope: CommandScope::Builtin,
                argument_hint: Some("<task>".into()),
                template: None,
            },
            SlashCommand {
                name: "plan".into(),
                description: "Plan carefully".into(),
                scope: CommandScope::Builtin,
                argument_hint: None,
                template: None,
            },
        ];
        let bridged = vec![SlashCommand {
            name: "usage".into(),
            description: "Show usage".into(),
            scope: CommandScope::Builtin,
            argument_hint: None,
            template: None,
        }];
        let merged = merge_antigravity_catalog(acp, skills, bridged);
        let swarm = merged
            .iter()
            .find(|command| command.name == "swarm")
            .expect("swarm should be present");
        assert_eq!(
            swarm.scope,
            CommandScope::Builtin,
            "ACP builtin must not be downgraded to Skill"
        );
        assert_eq!(swarm.argument_hint.as_deref(), Some("<task>"));
        assert_eq!(
            swarm.description, "Skill swarm description",
            "missing ACP description should be backfilled from skill metadata"
        );
        let boost = merged
            .iter()
            .find(|command| command.name == "boost")
            .expect("boost should be present");
        assert_eq!(
            boost.argument_hint.as_deref(),
            Some("<prompt>"),
            "explicit ACP argument_hint must be preserved"
        );
        let plan = merged
            .iter()
            .find(|command| command.name == "plan")
            .expect("plan should be present");
        assert_eq!(
            plan.argument_hint.as_deref(),
            Some("[<task>]"),
            "missing ACP argument_hint should be backfilled"
        );
        assert!(
            !merged.iter().any(|c| c.name == "teamwork"),
            "unadvertised commands must not be invented"
        );
        assert!(merged.iter().any(|c| c.name == "usage" && c.scope == CommandScope::Builtin));
        assert!(merged
            .iter()
            .any(|c| c.name == "antigravity-guide" && c.scope == CommandScope::Skill));
        assert_eq!(merged.len(), 5);
    }

    #[test]
    fn antigravity_bridged_cli_commands_match_client_bridge_contract() {
        let bridged = antigravity_bridged_cli_commands();
        let names = bridged
            .iter()
            .map(|command| (command.name.as_str(), command.argument_hint.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("usage", None),
                ("quota", None),
                ("credits", None),
                ("model", Some("[<model>]")),
                ("effort", Some("[low|medium|high|xhigh]")),
            ]
        );
        assert!(bridged.iter().all(|command| {
            command.scope == CommandScope::Builtin
                && command.template.is_none()
                && !command.description.is_empty()
        }));
    }

    #[test]
    fn droid_bridged_cli_commands_match_client_bridge_contract_and_discover_without_spawning() {
        let bridged = droid_bridged_cli_commands();
        let names = bridged
            .iter()
            .map(|command| (command.name.as_str(), command.argument_hint.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("model", Some("[model]")),
                ("reasoning", Some("[level]")),
                ("effort", Some("[level]")),
                ("autonomy", Some("[normal|auto-low|auto-medium|auto-high]")),
                ("new", None),
                ("clear", None),
            ]
        );
        assert!(bridged.iter().all(|command| {
            command.scope == CommandScope::Builtin
                && command.template.is_none()
                && !command.description.is_empty()
        }));

        // Cold-start discovery must return the bridged catalog without spawning
        // the CLI binary or creating a throwaway provider session.
        let discovered = discover(
            ProviderKind::Droid,
            Path::new("/nonexistent/waku-test-droid"),
            Path::new("/nonexistent/waku-test-project"),
            None,
        )
        .expect("Droid cold-start discovery must return bridged commands");
        assert_eq!(discovered, bridged);
    }

    #[test]
    fn antigravity_capture_command_injects_settings_before_prompt_and_account_env() {
        let dir = std::env::temp_dir().join(format!(
            "waku-agy-skills-cmd-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let settings_path = dir.join("settings.json");
        std::fs::write(&settings_path, "{}").unwrap();
        std::fs::write(dir.join(".env"), "GEMINI_API_KEY=test-key\n").unwrap();

        let command = build_capture_command(
            Path::new("agy"),
            &["-p", "/skills", "--output-format", "json"],
            &dir,
            Some(ProviderKind::Antigravity),
            Some(&dir),
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--settings".to_owned(),
                settings_path.to_string_lossy().into_owned(),
                "-p".to_owned(),
                "/skills".to_owned(),
                "--output-format".to_owned(),
                "json".to_owned(),
            ]
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == "GEMINI_API_KEY" && value.and_then(|v| v.to_str()) == Some("test-key")
        }));

        // Non-Antigravity callers without config_dir keep args unchanged.
        let plain = build_capture_command(
            Path::new("amp"),
            &["skill", "list", "--json"],
            &dir,
            None,
            None,
        );
        let plain_args: Vec<_> = plain
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(plain_args, vec!["skill", "list", "--json"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn antigravity_isolated_account_skips_cli_skills_probe() {
        let dir = std::path::PathBuf::from("/tmp/waku-agy-isolated");
        let mut cli_probed = false;
        let isolated = discover_antigravity_with(
            Some(&dir),
            || Some(vec![command("boost", Some("Boost"), CommandScope::Builtin, None, None).unwrap()]),
            || {
                cli_probed = true;
                Some(vec![command("default-only", Some("Leak"), CommandScope::Skill, None, None).unwrap()])
            },
        )
        .expect("isolated discovery still surfaces ACP and bridged commands");
        assert!(!cli_probed, "isolated account must not invoke `agy -p /skills`");
        assert!(!isolated.iter().any(|cmd| cmd.name == "default-only"));
        assert!(isolated.iter().any(|cmd| cmd.name == "boost"));
        assert!(isolated.iter().any(|cmd| cmd.name == "model"));

        let mut default_cli_probed = false;
        let default_catalog = discover_antigravity_with(
            None,
            || None,
            || {
                default_cli_probed = true;
                Some(vec![command("swarm", Some("Swarm"), CommandScope::Skill, None, None).unwrap()])
            },
        )
        .expect("default account discovery still runs `/skills`");
        assert!(default_cli_probed);
        assert!(default_catalog.iter().any(|cmd| cmd.name == "swarm"));
    }

    #[test]
    #[ignore = "requires an installed opencode; reads a cold workspace catalogue without creating sessions"]
    fn opencode_builtin_catalog_against_a_real_service() {
        let binary =
            crate::command_env::find_executable("opencode").expect("opencode is not installed");
        let root =
            std::env::temp_dir().join(format!("waku-command-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".opencode/commands")).unwrap();
        std::fs::write(
            root.join(".opencode/commands/waku-catalog-smoke.md"),
            "---\ndescription: Catalog test\n---\nReply OK",
        )
        .unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let commands = discover(ProviderKind::OpenCode, &binary, &root, None).unwrap();
        for name in ["init", "review", "waku-catalog-smoke"] {
            assert!(
                commands
                    .iter()
                    .any(|command| command.name == name && command.template.is_none()),
                "missing {name}"
            );
        }
        let service = crate::opencode_service::shared(&binary).unwrap();
        let (sessions, _) = crate::opencode_api::list_sessions(
            &service.endpoint(),
            Some(&root.to_string_lossy()),
            None,
            crate::opencode_api::Order::Desc,
            10,
            None,
        )
        .unwrap();
        assert!(
            sessions.is_empty(),
            "catalogue discovery must not create sessions"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
