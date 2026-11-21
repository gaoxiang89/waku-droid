//! Daemon-only provider discovery layered over shared protocol models.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub use waku_protocol::model::*;

/// Upper bound on `<cli> --version` probes so a hung CLI binary cannot stall
/// provider detection indefinitely.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// dk's launcher for clients that cannot call its `droid` shell alias or
/// PowerShell function: it injects the current dk key and runs the real CLI.
const DROID_DK_SHIM: &str = "droid-dk";

/// The executable for `provider` when the user has not set an override.
///
/// Droid prefers dk's shim when both it and the real CLI are installed, so a
/// dk user gets the same key in Waku as in their terminal. Setting the
/// override to `droid` opts back out.
pub fn default_provider_executable(provider: ProviderKind) -> Option<PathBuf> {
    let executable = crate::command_env::find_executable(provider.command());
    if provider == ProviderKind::Droid
        && executable.is_some()
        && let Some(shim) = droid_dk_shim()
    {
        return Some(shim.to_path_buf());
    }
    executable
}

/// Looked up once per daemon run: installing or removing dk mid-run is rare,
/// and a restart picks it up.
fn droid_dk_shim() -> Option<&'static Path> {
    static SHIM: OnceLock<Option<PathBuf>> = OnceLock::new();
    SHIM.get_or_init(|| crate::command_env::find_executable(DROID_DK_SHIM))
        .as_deref()
}

pub fn provider_probe(provider: ProviderKind, binary_override: Option<&str>) -> ProviderProbe {
    let path = match binary_override {
        Some(binary) => crate::command_env::resolve_binary_override(binary),
        None => default_provider_executable(provider),
    };
    ProviderProbe {
        provider,
        installed: path.is_some(),
        path,
        models: crate::model_catalog::fallback_models(provider),
        agent_presets: crate::model_catalog::fallback_agent_presets(provider),
        discovery_error: None,
    }
}

/// Detect a provider and hydrate its catalog from the daemon-owned cache.
///
/// This is the fast half of stale-while-revalidate: clients can render the
/// last successful catalog immediately, then request live discovery to replace
/// it. Cache I/O stays in the daemon instead of leaking host filesystem access
/// into desktop or Web clients.
pub fn cached_provider_probe(
    provider: ProviderKind,
    binary_override: Option<&str>,
) -> ProviderProbe {
    let cached = crate::model_catalog::cached_models(provider);
    apply_cached_models(provider_probe(provider, binary_override), cached)
}

fn apply_cached_models(
    mut probe: ProviderProbe,
    cached_models: Option<Vec<ProviderModel>>,
) -> ProviderProbe {
    if probe.provider.supports_model_discovery()
        && let Some(models) = cached_models
    {
        probe.models = models;
    }
    probe
}

pub fn discover_provider_models(probe: ProviderProbe) -> ProviderProbe {
    discover_provider_models_in(probe, None)
}

pub fn discover_provider_models_in(
    mut probe: ProviderProbe,
    config_dir: Option<&Path>,
) -> ProviderProbe {
    if probe.provider.supports_model_discovery()
        && let Some(path) = probe.path.as_deref()
    {
        let (models, agent_presets, discovery_error) =
            crate::model_catalog::discover_catalog_reporting_in(probe.provider, path, config_dir);
        probe.models = models;
        probe.agent_presets = agent_presets;
        probe.discovery_error = discovery_error;
    }
    probe
}

/// Run `<cli> --version` on the daemon host and extract its first version-like
/// token. Provider CLIs decorate this output differently, so clients receive a
/// normalized value rather than subprocess output.
pub fn probe_provider_version(binary: &Path) -> Option<String> {
    probe_provider_version_with_timeout(binary, VERSION_PROBE_TIMEOUT)
}

fn probe_provider_version_with_timeout(binary: &Path, timeout: Duration) -> Option<String> {
    let mut command = crate::command_env::command(binary);
    let command = command.arg("--version");
    let output = crate::command_env::output_with_timeout(command, timeout).ok()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_cli_version(&combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_catalog_replaces_fallback_before_live_discovery() {
        let probe = ProviderProbe {
            provider: ProviderKind::Codex,
            installed: true,
            path: Some("/usr/bin/codex".into()),
            models: crate::model_catalog::fallback_models(ProviderKind::Codex),
            agent_presets: Vec::new(),
            discovery_error: None,
        };
        let cached = vec![ProviderModel::new("cached-model", "Cached model").default()];

        let probe = apply_cached_models(probe, Some(cached));

        assert_eq!(probe.models.len(), 1);
        assert_eq!(probe.models[0].id, "cached-model");
    }

    #[test]
    fn probe_provider_version_reads_output_and_bounds_hanging_binaries() {
        let dir = std::env::temp_dir().join(format!("waku-version-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        #[cfg(windows)]
        let (ok_bin, ok_script, hang_bin, hang_script) = (
            dir.join("ok-cli.cmd"),
            "@echo off\r\necho faux-cli 2.4.1\r\n",
            dir.join("hang-cli.cmd"),
            "@echo off\r\nping -n 30 127.0.0.1 >nul\r\n",
        );
        #[cfg(not(windows))]
        let (ok_bin, ok_script, hang_bin, hang_script) = (
            dir.join("ok-cli"),
            "#!/bin/sh\nprintf 'faux-cli 2.4.1\\n'\n",
            dir.join("hang-cli"),
            "#!/bin/sh\nsleep 30\n",
        );

        std::fs::write(&ok_bin, ok_script).unwrap();
        std::fs::write(&hang_bin, hang_script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [&ok_bin, &hang_bin] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        assert_eq!(
            probe_provider_version_with_timeout(&ok_bin, Duration::from_secs(10)).as_deref(),
            Some("2.4.1")
        );

        let started = std::time::Instant::now();
        assert_eq!(
            probe_provider_version_with_timeout(&hang_bin, Duration::from_millis(250)),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "hanging version probe did not time out promptly: {:?}",
            started.elapsed()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
