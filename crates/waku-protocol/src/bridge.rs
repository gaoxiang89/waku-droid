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
    fn fingerprint_is_independent_of_chunking() {
        let mut whole = Fingerprint::default();
        whole.update(b"waku-daemon");
        let mut split = Fingerprint::default();
        split.update(b"waku-");
        split.update(b"daemon");
        assert_eq!(whole.finish(), split.finish());
    }
}
