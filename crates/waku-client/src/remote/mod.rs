//! Reaching daemons on WSL distributions and SSH hosts.
//!
//! Remote hosts run the same `waku-daemon` as this machine, so providers,
//! files, Git, and terminals all execute where the code lives. This module
//! only decides how to install that daemon and carry its WebSocket stream:
//! through `wsl.exe` or the user's `ssh`, never a separate network listener.

pub mod bridge;
pub mod connection;
pub mod credentials;
pub mod deploy;
mod process_util;
pub mod ssh;
pub mod wsl;

pub use bridge::{BridgeError, BridgeSession, BridgeStage, test_connection};
pub use connection::{
    Connection, ConnectionKind, ConnectionsFile, LOCAL_CONNECTION_ID, SshAuthMethod, SshTarget,
};
