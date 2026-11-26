use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::computer_use::ComputerAppGrant;
use crate::model::ProviderKind;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccount {
    pub id: String,
    pub provider: ProviderKind,
    pub label: String,
    pub config_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_override: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(default)]
pub struct DaemonSettings {
    pub computer_use_enabled: bool,
    pub computer_use_allowed_apps: Vec<ComputerAppGrant>,
    pub disabled_providers: Vec<ProviderKind>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub provider_binary_overrides: HashMap<ProviderKind, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_accounts: Vec<ProviderAccount>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            computer_use_enabled: false,
            computer_use_allowed_apps: Vec::new(),
            disabled_providers: Vec::new(),
            provider_binary_overrides: HashMap::new(),
            provider_accounts: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl DaemonSettings {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".narel")
            .join("settings.json")
    }

    pub fn discard_legacy_app_keys(&mut self) {
        for key in ["analytics_enabled", "favorite_models", "theme", "language"] {
            self.extra.remove(key);
        }
    }

    pub fn accounts_for(&self, provider: ProviderKind) -> impl Iterator<Item = &ProviderAccount> {
        self.provider_accounts
            .iter()
            .filter(move |account| account.provider == provider)
    }

    pub fn account(
        &self,
        provider: ProviderKind,
        account_id: Option<&str>,
    ) -> Option<&ProviderAccount> {
        let account_id = account_id?.trim();
        if account_id.is_empty() {
            return None;
        }
        self.provider_accounts
            .iter()
            .find(|account| account.provider == provider && account.id == account_id)
    }

    pub fn effective_binary_override(
        &self,
        provider: ProviderKind,
        account_id: Option<&str>,
    ) -> Option<&str> {
        self.account(provider, account_id)
            .and_then(|account| account.binary_override.as_deref())
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.provider_binary_overrides
                    .get(&provider)
                    .map(String::as_str)
            })
    }

    pub fn effective_config_dir(
        &self,
        provider: ProviderKind,
        account_id: Option<&str>,
    ) -> Option<&str> {
        self.account(provider, account_id)
            .map(|account| account.config_dir.trim())
            .filter(|value| !value.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_settings() -> DaemonSettings {
        let mut settings = DaemonSettings::default();
        settings
            .provider_binary_overrides
            .insert(ProviderKind::Claude, "/usr/local/bin/claude".into());
        settings
            .provider_binary_overrides
            .insert(ProviderKind::Codex, "/usr/local/bin/codex".into());
        settings.provider_accounts = vec![
            ProviderAccount {
                id: "work".into(),
                provider: ProviderKind::Claude,
                label: "Work Claude".into(),
                config_dir: "  ~/.claude-work  ".into(),
                binary_override: Some("/opt/claude-work".into()),
            },
            ProviderAccount {
                id: "blank-binary".into(),
                provider: ProviderKind::Claude,
                label: "Blank Binary".into(),
                config_dir: "~/.claude-alt".into(),
                binary_override: Some("   ".into()),
            },
            ProviderAccount {
                id: "blank-dir".into(),
                provider: ProviderKind::Claude,
                label: "Blank Dir".into(),
                config_dir: "   ".into(),
                binary_override: None,
            },
            ProviderAccount {
                id: "work".into(),
                provider: ProviderKind::Codex,
                label: "Work Codex".into(),
                config_dir: "~/.codex-work".into(),
                binary_override: None,
            },
        ];
        settings
    }

    #[test]
    fn account_lookup_matches_provider_and_trims_id() {
        let settings = sample_settings();

        let claude_work = settings
            .account(ProviderKind::Claude, Some("  work  "))
            .expect("trimmed account id matches");
        assert_eq!(claude_work.label, "Work Claude");

        let codex_work = settings
            .account(ProviderKind::Codex, Some("work"))
            .expect("same id under a different provider is distinct");
        assert_eq!(codex_work.label, "Work Codex");

        assert_eq!(settings.account(ProviderKind::Claude, None), None);
        assert_eq!(settings.account(ProviderKind::Claude, Some("")), None);
        assert_eq!(settings.account(ProviderKind::Claude, Some("   ")), None);
        assert_eq!(
            settings.account(ProviderKind::Claude, Some("missing")),
            None
        );
        assert_eq!(settings.account(ProviderKind::Grok, Some("work")), None);

        let claude_ids: Vec<&str> = settings
            .accounts_for(ProviderKind::Claude)
            .map(|account| account.id.as_str())
            .collect();
        assert_eq!(claude_ids, vec!["work", "blank-binary", "blank-dir"]);
    }

    #[test]
    fn effective_binary_override_prefers_non_empty_account_override_then_provider_default() {
        let settings = sample_settings();

        assert_eq!(
            settings.effective_binary_override(ProviderKind::Claude, Some("work")),
            Some("/opt/claude-work")
        );
        // Whitespace-only or absent account override falls back to provider override.
        assert_eq!(
            settings.effective_binary_override(ProviderKind::Claude, Some("blank-binary")),
            Some("/usr/local/bin/claude")
        );
        assert_eq!(
            settings.effective_binary_override(ProviderKind::Codex, Some("work")),
            Some("/usr/local/bin/codex")
        );
        // Default account or unknown account falls back to provider override.
        assert_eq!(
            settings.effective_binary_override(ProviderKind::Claude, None),
            Some("/usr/local/bin/claude")
        );
        assert_eq!(
            settings.effective_binary_override(ProviderKind::Claude, Some("missing")),
            Some("/usr/local/bin/claude")
        );
        // Provider without any override yields None.
        assert_eq!(
            settings.effective_binary_override(ProviderKind::Grok, None),
            None
        );
    }

    #[test]
    fn effective_config_dir_trims_and_rejects_empty_values() {
        let settings = sample_settings();

        assert_eq!(
            settings.effective_config_dir(ProviderKind::Claude, Some("work")),
            Some("~/.claude-work")
        );
        assert_eq!(
            settings.effective_config_dir(ProviderKind::Codex, Some("work")),
            Some("~/.codex-work")
        );
        assert_eq!(
            settings.effective_config_dir(ProviderKind::Claude, Some("blank-dir")),
            None
        );
        assert_eq!(
            settings.effective_config_dir(ProviderKind::Claude, None),
            None
        );
        assert_eq!(
            settings.effective_config_dir(ProviderKind::Claude, Some("missing")),
            None
        );
        assert_eq!(
            settings.effective_config_dir(ProviderKind::Grok, Some("work")),
            None
        );
    }

    #[test]
    fn provider_account_and_daemon_settings_serialization_round_trip() {
        let with_override = ProviderAccount {
            id: "work".into(),
            provider: ProviderKind::Claude,
            label: "Work".into(),
            config_dir: "~/.claude-work".into(),
            binary_override: Some("/opt/claude".into()),
        };
        let json = serde_json::to_value(&with_override).unwrap();
        assert_eq!(json["id"], "work");
        assert_eq!(json["provider"], "claude");
        assert_eq!(json["label"], "Work");
        assert_eq!(json["configDir"], "~/.claude-work");
        assert_eq!(json["binaryOverride"], "/opt/claude");
        assert_eq!(
            serde_json::from_value::<ProviderAccount>(json).unwrap(),
            with_override
        );

        let without_override = ProviderAccount {
            id: "personal".into(),
            provider: ProviderKind::Codex,
            label: "Personal".into(),
            config_dir: "~/.codex-personal".into(),
            binary_override: None,
        };
        let json = serde_json::to_value(&without_override).unwrap();
        assert!(json.get("binaryOverride").is_none());
        assert_eq!(
            serde_json::from_value::<ProviderAccount>(json).unwrap(),
            without_override
        );

        let default_json = serde_json::to_value(DaemonSettings::default()).unwrap();
        assert!(default_json.get("provider_accounts").is_none());
        assert!(default_json.get("provider_binary_overrides").is_none());

        let mut settings = sample_settings();
        settings
            .extra
            .insert("theme".into(), Value::String("dark".into()));
        settings
            .extra
            .insert("custom_flag".into(), Value::Bool(true));
        settings.discard_legacy_app_keys();
        assert!(!settings.extra.contains_key("theme"));
        assert_eq!(settings.extra.get("custom_flag"), Some(&Value::Bool(true)));

        let encoded = serde_json::to_string(&settings).unwrap();
        let decoded: DaemonSettings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, settings);
    }
}
