//! Contract between the desktop and `waku-daemon bridge` on a remote host.
//!
//! A WSL distribution or SSH host runs one long-lived daemon bound to its own
//! loopback interface. The desktop reaches it by launching `waku-daemon
//! bridge` through `wsl.exe` or `ssh`: the bridge starts or reuses that daemon,
//! announces it with one [`BridgeReady`] line on stdout, and then relays raw
//! WebSocket bytes between its stdio and the daemon's socket. The daemon
//! therefore outlives a dropped connection, and the desktop resumes its
//! runtimes through the ordinary replay cursors.

use serde::{Deserialize, Serialize};

/// The subcommand that turns `waku-daemon` into a stdio relay.
pub const BRIDGE_SUBCOMMAND: &str = "bridge";
/// Prints the binary's content fingerprint and exits.
pub const FINGERPRINT_FLAG: &str = "--fingerprint";
/// Bridge flag: end a running daemon that speaks another protocol, and its
/// sessions, instead of refusing to start. Only an explicit user
/// confirmation may pass it; reconnects never do.
pub const REPLACE_INCOMPATIBLE_FLAG: &str = "--replace-incompatible";

/// First stdout line of `waku-daemon bridge`. Everything after it is the
/// relayed WebSocket stream.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeReady {
    pub token: String,
    pub protocol_version: u32,
    pub fingerprint: String,
    pub pid: u32,
}

/// Sole stdout line of a bridge that refused to replace a running daemon
/// speaking another protocol. The desktop cannot talk to that daemon, so the
/// bridge describes it from the operating system's process table instead.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeBlocked {
    pub incompatible_daemon: IncompatibleDaemon,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IncompatibleDaemon {
    pub pid: u32,
    pub protocol_version: u32,
    /// The daemon's descendants (terminal shells, agents, and what they
    /// run), depth-first, capped at [`IncompatibleDaemon::MAX_PROCESSES`].
    pub processes: Vec<DaemonProcess>,
    /// How many descendants did not fit in `processes`.
    #[serde(default)]
    pub omitted: u32,
}

impl IncompatibleDaemon {
    pub const MAX_PROCESSES: usize = 24;
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonProcess {
    pub pid: u32,
    /// 0 for the daemon's direct children.
    pub depth: u32,
    pub command: String,
}

/// Daemon-owned record of a bridge-managed listener. The daemon writes it
/// with owner-only permissions after binding; bridges read it to reuse the
/// daemon instead of starting another one.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeState {
    pub address: String,
    pub token: String,
    pub pid: u32,
    pub protocol_version: u32,
    pub fingerprint: String,
}

/// 64-bit FNV-1a over a binary's bytes, rendered as 16 hex digits.
///
/// Deployment trusts content, not file names or version strings: an installed
/// daemon is reused only when it hashes exactly like the build the desktop
/// carries. The hash is not a security boundary; the transport already is.
#[derive(Clone, Copy, Debug)]
pub struct Fingerprint(u64);

impl Default for Fingerprint {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fingerprint {
    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn finish(self) -> String {
        format!("{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_reference_fnv1a() {
        let mut empty = Fingerprint::default();
        empty.update(b"");
        assert_eq!(empty.finish(), "cbf29ce484222325");
        let mut text = Fingerprint::default();
        text.update(b"a");
        assert_eq!(text.finish(), "af63dc4c8601ec8c");
    }

    #[test]
    fn blocked_line_never_parses_as_ready() {
        let blocked = BridgeBlocked {
            incompatible_daemon: IncompatibleDaemon {
                pid: 7,
                protocol_version: 7,
                processes: vec![DaemonProcess {
                    pid: 8,
                    depth: 0,
                    command: "bash".into(),
                }],
                omitted: 0,
            },
        };
        let line = serde_json::to_string(&blocked).unwrap();
        assert!(serde_json::from_str::<BridgeReady>(&line).is_err());
        assert_eq!(serde_json::from_str::<BridgeBlocked>(&line).unwrap(), blocked);
    }

    #[test]
    fn fingerprint_is_independent_of_chunking() {
        let mut whole = Fingerprint::default();
        whole.update(b"waku-daemon");
        let mut split = Fingerprint::default();
        split.update(b"waku-");
        split.update(b"daemon");
        assert_eq!(whole.finish(), split.finish());
    }
}
