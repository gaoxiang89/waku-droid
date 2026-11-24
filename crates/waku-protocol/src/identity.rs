//! Shared application identity used by the daemon and desktop client.

#[cfg(debug_assertions)]
pub const APP_NAME: &str = "Narel Debug";
#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Narel";

#[cfg(debug_assertions)]
pub const APP_ID: &str = "sh.narel.dev";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "sh.narel";

#[cfg(debug_assertions)]
pub const DATA_DIRECTORY_NAME: &str = "Narel Debug";
#[cfg(not(debug_assertions))]
pub const DATA_DIRECTORY_NAME: &str = "Narel";
