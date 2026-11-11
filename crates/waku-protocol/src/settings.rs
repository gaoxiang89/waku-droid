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
            .join(".waku")
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
