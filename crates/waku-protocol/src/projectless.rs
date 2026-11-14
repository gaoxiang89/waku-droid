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
        && !roots.all().any(|existing| lexical_eq(existing, &root))
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
    home_from_root(&root)
}

pub fn is_projectless_path(path: &Path) -> bool {
    is_projectless_in_roots(&workspace_roots(), path)
}

pub fn needs_migration(path: &Path) -> bool {
    needs_migration_in_roots(&workspace_roots(), path)
}

pub fn is_legacy_root_path(path: &Path) -> bool {
    is_legacy_root_in_roots(&workspace_roots(), path)
}

fn home_from_root(root: &Path) -> Option<PathBuf> {
    let dot_waku = lexical_parent(root)?;
    lexical_parent(&dot_waku)
}

fn is_projectless_in_roots(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| {
        lexical_starts_with(path, root)
            || lexical_parent(root)
                .is_some_and(|legacy_root| is_legacy_workspace_path(path, &legacy_root))
    })
}

fn needs_migration_in_roots(roots: &[PathBuf], path: &Path) -> bool {
    !roots.iter().any(|root| lexical_starts_with(path, root))
        && roots.iter().any(|root| {
            lexical_parent(root)
                .is_some_and(|legacy_root| is_legacy_workspace_path(path, &legacy_root))
        })
}

fn is_legacy_root_in_roots(roots: &[PathBuf], path: &Path) -> bool {
    roots
        .iter()
        .any(|root| lexical_parent(root).is_some_and(|legacy_root| lexical_eq(path, &legacy_root)))
}

fn is_legacy_workspace_path(path: &Path, legacy_root: &Path) -> bool {
    if lexical_eq(path, legacy_root) {
        return true;
    }
    let Some(rest) = lexical_strip_prefix(path, legacy_root) else {
        return false;
    };
    let Some(date) = rest.first().copied() else {
        return false;
    };
    is_date_component(date)
}

#[derive(Clone, Copy)]
enum LexicalRoot<'a> {
    Posix,
    WindowsDrive(&'a str),
    WindowsUnc(&'a str, &'a str),
}

fn parse_lexical(path: &Path) -> Option<(LexicalRoot<'_>, Vec<&str>)> {
    let raw = path.to_str()?;
    let unc_tail = raw
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| {
            (!raw.starts_with(r"\\?\"))
                .then(|| raw.strip_prefix(r"\\"))
                .flatten()
        })
        .or_else(|| raw.strip_prefix("//"));
    if let Some(unc_tail) = unc_tail {
        let mut parts = unc_tail
            .split(['\\', '/'])
            .filter(|part| !part.is_empty() && *part != ".");
        let server = parts.next()?;
        let share = parts.next()?;
        return Some((LexicalRoot::WindowsUnc(server, share), parts.collect()));
    }
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || matches!(bytes[2], b'\\' | b'/'))
    {
        let segs = trimmed[2..]
            .split(['\\', '/'])
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        return Some((LexicalRoot::WindowsDrive(&trimmed[..2]), segs));
    }
    if trimmed.starts_with('/') && !trimmed.starts_with("//") {
        let segs = trimmed
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        return Some((LexicalRoot::Posix, segs));
    }
    None
}

fn format_lexical(root: LexicalRoot<'_>, segs: &[&str]) -> PathBuf {
    match root {
        LexicalRoot::Posix => {
            if segs.is_empty() {
                PathBuf::from("/")
            } else {
                PathBuf::from(format!("/{}", segs.join("/")))
            }
        }
        LexicalRoot::WindowsDrive(drive) => {
            if segs.is_empty() {
                PathBuf::from(format!(r"{drive}\"))
            } else {
                PathBuf::from(format!(r"{drive}\{}", segs.join(r"\")))
            }
        }
        LexicalRoot::WindowsUnc(server, share) => {
            if segs.is_empty() {
                PathBuf::from(format!(r"\\{server}\{share}"))
            } else {
                PathBuf::from(format!(r"\\{server}\{share}\{}", segs.join(r"\")))
            }
        }
    }
}

fn roots_match(a: LexicalRoot<'_>, b: LexicalRoot<'_>) -> bool {
    match (a, b) {
        (LexicalRoot::Posix, LexicalRoot::Posix) => true,
        (LexicalRoot::WindowsDrive(da), LexicalRoot::WindowsDrive(db)) => {
            da.eq_ignore_ascii_case(db)
        }
        (LexicalRoot::WindowsUnc(sa, sha), LexicalRoot::WindowsUnc(sb, shb)) => {
            sa.eq_ignore_ascii_case(sb) && sha.eq_ignore_ascii_case(shb)
        }
        _ => false,
    }
}

fn segments_equal(root: LexicalRoot<'_>, a: &[&str], b: &[&str]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    match root {
        LexicalRoot::Posix => a == b,
        LexicalRoot::WindowsDrive(_) | LexicalRoot::WindowsUnc(_, _) => {
            a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
        }
    }
}

fn lexical_parent(path: &Path) -> Option<PathBuf> {
    let Some((root, mut segs)) = parse_lexical(path) else {
        return path.parent().map(Path::to_path_buf);
    };
    segs.pop()?;
    Some(format_lexical(root, &segs))
}

fn lexical_strip_prefix<'a>(path: &'a Path, base: &Path) -> Option<Vec<&'a str>> {
    let (Some((p_root, p_segs)), Some((b_root, b_segs))) =
        (parse_lexical(path), parse_lexical(base))
    else {
        return path.strip_prefix(base).ok().map(|rel| {
            rel.components()
                .filter_map(|component| component.as_os_str().to_str())
                .collect()
        });
    };
    if !roots_match(p_root, b_root) || p_segs.len() < b_segs.len() {
        return None;
    }
    segments_equal(b_root, &p_segs[..b_segs.len()], &b_segs)
        .then(|| p_segs[b_segs.len()..].to_vec())
}

fn lexical_starts_with(path: &Path, base: &Path) -> bool {
    lexical_strip_prefix(path, base).is_some()
}

fn lexical_eq(a: &Path, b: &Path) -> bool {
    let (Some((a_root, a_segs)), Some((b_root, b_segs))) = (parse_lexical(a), parse_lexical(b))
    else {
        return a == b;
    };
    roots_match(a_root, b_root) && segments_equal(a_root, &a_segs, &b_segs)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_windows_and_posix_projectless_paths_lexically() {
        let win_root = PathBuf::from(r"C:\Users\Alice\.waku\projects");
        let unc_root = PathBuf::from(r"\\srv\share\Alice\.waku\projects");
        let posix_root = PathBuf::from("/home/alice/.waku/projects");
        let roots = vec![win_root.clone(), unc_root.clone(), posix_root.clone()];

        assert_eq!(
            home_from_root(&win_root),
            Some(PathBuf::from(r"C:\Users\Alice"))
        );
        assert_eq!(
            home_from_root(&unc_root),
            Some(PathBuf::from(r"\\srv\share\Alice"))
        );
        assert_eq!(
            home_from_root(&posix_root),
            Some(PathBuf::from("/home/alice"))
        );

        assert!(is_projectless_in_roots(
            &roots,
            Path::new("c:/users/alice/.waku/projects/session-1")
        ));
        assert!(is_projectless_in_roots(
            &roots,
            Path::new(r"\\SRV\Share\alice\.waku\projects\session-1")
        ));
        assert!(is_projectless_in_roots(
            &roots,
            Path::new("/home/alice/.waku/projects/session-1")
        ));
        assert!(!is_projectless_in_roots(
            &roots,
            Path::new(r"C:\Users\Alice\code\waku")
        ));
        assert!(!is_projectless_in_roots(
            &roots,
            Path::new("/home/alice/code/waku")
        ));

        let win_legacy = Path::new(r"c:\users\alice\.waku\2026-04-01\foo");
        let posix_legacy = Path::new("/home/alice/.waku/2026-04-01/foo");
        assert!(is_projectless_in_roots(&roots, win_legacy));
        assert!(needs_migration_in_roots(&roots, win_legacy));
        assert!(is_projectless_in_roots(&roots, posix_legacy));
        assert!(needs_migration_in_roots(&roots, posix_legacy));
        assert!(!needs_migration_in_roots(
            &roots,
            Path::new(r"C:\Users\Alice\.waku\projects\2026-04-01\foo")
        ));

        assert!(is_legacy_root_in_roots(
            &roots,
            Path::new(r"c:\users\alice\.waku")
        ));
        assert!(is_legacy_root_in_roots(
            &roots,
            Path::new("/home/alice/.waku")
        ));
        assert!(!is_legacy_root_in_roots(
            &roots,
            Path::new(r"C:\Users\Alice")
        ));
    }
}
