//! Where a daemon runs: this machine, a WSL distribution, or an SSH host.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const LOCAL_CONNECTION_ID: &str = "local";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// Stable identity: `local`, `wsl:<distro>`, or `ssh:<random>`.
    pub id: String,
    pub name: String,
    pub kind: ConnectionKind,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectionKind {
    Local,
    Wsl { distro: String },
    Ssh { target: SshTarget },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SshTarget {
    /// A `Host` alias from `~/.ssh/config`; ssh resolves everything else.
    ConfigHost {
        host: String,
        #[serde(default)]
        auth_method: SshAuthMethod,
    },
    Direct {
        hostname: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        identity_file: Option<String>,
        #[serde(default)]
        auth_method: SshAuthMethod,
        #[serde(default)]
        proxy_jump: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SshAuthMethod {
    /// ssh-agent and the user's ssh config decide.
    #[default]
    Agent,
    /// A password kept in the system credential store and answered through
    /// Waku's own askpass mode.
    Password,
    /// An explicit private key, offered exclusively.
    Key,
}

impl SshTarget {
    pub fn auth_method(&self) -> SshAuthMethod {
        match self {
            Self::ConfigHost { auth_method, .. } | Self::Direct { auth_method, .. } => *auth_method,
        }
    }

    /// Short host label for UI and diagnostics.
    pub fn display_host(&self) -> String {
        match self {
            Self::ConfigHost { host, .. } => host.clone(),
            Self::Direct {
                hostname,
                user,
                port,
                ..
            } => {
                let mut label = match user {
                    Some(user) => format!("{user}@{hostname}"),
                    None => hostname.clone(),
                };
                if let Some(port) = port {
                    label.push_str(&format!(":{port}"));
                }
                label
            }
        }
    }
}

impl Connection {
    pub fn local() -> Self {
        Self {
            id: LOCAL_CONNECTION_ID.into(),
            name: "Local".into(),
            kind: ConnectionKind::Local,
        }
    }

    /// WSL connections are keyed by distribution, so adding one twice is an
    /// idempotent update.
    pub fn wsl(distro: &str) -> Self {
        Self {
            id: format!("wsl:{distro}"),
            name: format!("WSL · {distro}"),
            kind: ConnectionKind::Wsl {
                distro: distro.to_owned(),
            },
        }
    }

    pub fn ssh(target: SshTarget) -> Self {
        Self {
            id: format!("ssh:{}", uuid::Uuid::new_v4().simple()),
            name: format!("SSH · {}", target.display_host()),
            kind: ConnectionKind::Ssh { target },
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self.kind, ConnectionKind::Local)
    }

    /// Parse the `WAKU_CONNECTION` shorthand: `wsl:<distro>`, `ssh:<config
    /// host>`, or the id of a saved connection.
    pub fn from_spec(spec: &str, saved: &[Connection]) -> Option<Self> {
        let spec = spec.trim();
        if let Some(connection) = saved.iter().find(|connection| connection.id == spec) {
            return Some(connection.clone());
        }
        if spec == LOCAL_CONNECTION_ID {
            return Some(Self::local());
        }
        if let Some(distro) = spec.strip_prefix("wsl:").filter(|value| !value.is_empty()) {
            return Some(Self::wsl(distro));
        }
        let host = spec
            .strip_prefix("ssh:")
            .filter(|value| !value.is_empty())?;
        let mut connection = Self::ssh(SshTarget::ConfigHost {
            host: host.to_owned(),
            auth_method: SshAuthMethod::Agent,
        });
        connection.id = format!("ssh:{host}");
        Some(connection)
    }
}

/// `connections.json`: saved remote connections and the one the desktop
/// connects to at launch. Saving never contacts a host.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionsFile {
    pub connections: Vec<Connection>,
    /// Saved connection id the desktop opens at launch; `None` is local.
    pub active: Option<String>,
}

impl ConnectionsFile {
    pub fn path() -> PathBuf {
        crate::persistence::default_connections_path()
    }

    pub fn load() -> io::Result<Self> {
        match std::fs::read(Self::path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        crate::persistence::write_json_atomically(&Self::path(), self)
    }

    pub fn get(&self, id: &str) -> Option<&Connection> {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
    }

    pub fn upsert(&mut self, connection: Connection) {
        match self
            .connections
            .iter_mut()
            .find(|existing| existing.id == connection.id)
        {
            Some(existing) => *existing = connection,
            None => self.connections.push(connection),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<Connection> {
        let index = self
            .connections
            .iter()
            .position(|connection| connection.id == id)?;
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        Some(self.connections.remove(index))
    }

    pub fn active_connection(&self) -> Option<&Connection> {
        self.active.as_deref().and_then(|id| self.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_ssh_targets_default_to_agent_auth() {
        let target: SshTarget =
            serde_json::from_str(r#"{"type":"configHost","host":"devbox"}"#).unwrap();
        assert_eq!(target.auth_method(), SshAuthMethod::Agent);
        let target: SshTarget =
            serde_json::from_str(r#"{"type":"direct","hostname":"example.com"}"#).unwrap();
        assert_eq!(target.auth_method(), SshAuthMethod::Agent);
    }

    #[test]
    fn connection_kinds_round_trip_with_tags() {
        let connection = Connection::wsl("Ubuntu-24.04");
        let json = serde_json::to_string(&connection).unwrap();
        assert!(json.contains(r#""type":"wsl""#));
        assert_eq!(
            serde_json::from_str::<Connection>(&json).unwrap(),
            connection
        );
    }

    #[test]
    fn specs_resolve_saved_ids_before_shorthands() {
        let saved = vec![Connection {
            id: "ssh:abc".into(),
            name: "Build box".into(),
            kind: ConnectionKind::Ssh {
                target: SshTarget::ConfigHost {
                    host: "build".into(),
                    auth_method: SshAuthMethod::Key,
                },
            },
        }];
        assert_eq!(
            Connection::from_spec("ssh:abc", &saved).unwrap().name,
            "Build box"
        );
        assert_eq!(
            Connection::from_spec("wsl:Debian", &saved).unwrap().id,
            "wsl:Debian"
        );
        assert_eq!(
            Connection::from_spec("ssh:devbox", &saved).unwrap().id,
            "ssh:devbox"
        );
        assert!(Connection::from_spec("ssh:", &saved).is_none());
        assert!(Connection::from_spec("bogus", &saved).is_none());
    }

    #[test]
    fn removing_the_active_connection_falls_back_to_local() {
        let mut file = ConnectionsFile::default();
        file.upsert(Connection::wsl("Debian"));
        file.active = Some("wsl:Debian".into());
        file.upsert(Connection::wsl("Debian"));
        assert_eq!(file.connections.len(), 1);
        file.remove("wsl:Debian");
        assert!(file.active.is_none());
    }
}
