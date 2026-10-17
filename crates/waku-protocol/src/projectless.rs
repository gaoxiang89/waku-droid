//! Pure daemon-path classification shared by clients and the daemon.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// Projectless workspace roots. The primary root belongs to the daemon the
/// process was started for; desktop windows connected to WSL or SSH daemons
/// add theirs, so one process can classify paths from several hosts. Roots
/// from different hosts never nest, so matching any of them is unambiguous.
#[derive(Default)]
struct Roots {
    primary: Option<PathBuf>,
    additional: Vec<PathBuf>,
}

impl Roots {
    fn all(&self) -> impl Iterator<Item = &PathBuf> {
        self.primary.iter().chain(&self.additional)
    }
}

fn workspace_root_slot() -> &'static RwLock<Roots> {
    static ROOT: OnceLock<RwLock<Roots>> = OnceLock::new();
    ROOT.get_or_init(|| {
        RwLock::new(Roots {
            primary: dirs::home_dir().map(|home| home.join(".waku").join("projects")),
            additional: Vec::new(),
        })
    })
}

pub fn set_workspace_root(root: Option<PathBuf>) {
    if let Ok(mut roots) = workspace_root_slot().write() {
        roots.primary = root;
    }
}

/// Register another daemon's root without replacing the primary one.
pub fn add_workspace_root(root: PathBuf) {
    if let Ok(mut roots) = workspace_root_slot().write()
        && !roots.all().any(|existing| *existing == root)
    {
        roots.additional.push(root);
    }
}

pub fn workspace_root() -> Option<PathBuf> {
    workspace_root_slot().read().ok()?.primary.clone()
}

/// Every registered root, primary first.
pub fn workspace_roots() -> Vec<PathBuf> {
    workspace_root_slot()
        .read()
        .map(|roots| roots.all().cloned().collect())
        .unwrap_or_default()
}

pub fn home_directory() -> Option<PathBuf> {
    let root = workspace_root()?;
    root.parent()?.parent().map(Path::to_path_buf)
}

pub fn is_projectless_path(path: &Path) -> bool {
    workspace_roots().iter().any(|root| {
        path.starts_with(root)
            || root
                .parent()
                .is_some_and(|legacy_root| is_legacy_workspace_path(path, legacy_root))
    })
}

pub fn needs_migration(path: &Path) -> bool {
    let roots = workspace_roots();
    !roots.iter().any(|root| path.starts_with(root))
        && roots.iter().any(|root| {
            root.parent()
                .is_some_and(|legacy_root| is_legacy_workspace_path(path, legacy_root))
        })
}

pub fn is_legacy_root_path(path: &Path) -> bool {
    workspace_roots()
        .iter()
        .any(|root| root.parent().is_some_and(|legacy_root| path == legacy_root))
}

fn is_legacy_workspace_path(path: &Path, legacy_root: &Path) -> bool {
    if path == legacy_root {
        return true;
    }
    let Some(date) = path
        .strip_prefix(legacy_root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    is_date_component(date)
}

fn is_date_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}
