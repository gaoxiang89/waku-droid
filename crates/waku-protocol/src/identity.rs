//! Shared application identity used by the daemon and desktop client.

use std::path::{Path, PathBuf};

pub mod migration;

/// Unix settings and connection catalog: `~/.waku` → `~/.narel`.
pub fn legacy_home_data_pair(home: &Path) -> (PathBuf, PathBuf) {
    (home.join(".waku"), home.join(".narel"))
}

/// Platform local data, including Windows `%LOCALAPPDATA%\Waku` → `Narel`.
/// Debug builds use [`DATA_DIRECTORY_NAME`] (`Narel Debug`) so a checkout
/// never imports into the installed app's directory.
pub fn legacy_local_data_pair(local_data: &Path) -> (PathBuf, PathBuf) {
    (
        local_data.join("Waku"),
        local_data.join(DATA_DIRECTORY_NAME),
    )
}

/// Run before any preferences, connection catalog, or daemon store is opened.
/// Debug checkouts and explicitly isolated daemon data roots are not upgrades.
pub fn import_legacy_user_data() -> std::io::Result<()> {
    if cfg!(debug_assertions) {
        return Ok(());
    }
    if let Some(home) = dirs::home_dir() {
        let (source, destination) = legacy_home_data_pair(&home);
        migration::import_directory(&source, &destination)?;
    }
    if std::env::var_os("WAKU_DATA_DIR").is_none_or(|value| value.is_empty())
        && let Some(data) = dirs::data_local_dir()
    {
        let (source, destination) = legacy_local_data_pair(&data);
        migration::import_directory(&source, &destination)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_paths_cover_unix_home_and_windows_local_app_data() {
        let home = Path::new("/home/ada");
        let (source, destination) = legacy_home_data_pair(home);
        assert_eq!(source, home.join(".waku"));
        assert_eq!(destination, home.join(".narel"));

        let local = Path::new("C:/Users/ada/AppData/Local");
        let (source, destination) = legacy_local_data_pair(local);
        assert_eq!(source, local.join("Waku"));
        assert_eq!(
            destination.file_name().and_then(|name| name.to_str()),
            Some(DATA_DIRECTORY_NAME)
        );
        #[cfg(not(debug_assertions))]
        assert_eq!(destination, local.join("Narel"));
    }
}

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
