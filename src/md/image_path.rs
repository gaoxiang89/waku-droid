//! Where a Markdown image's bytes live.
//!
//! Classifying an image URL is pure string work, so render can do it every
//! frame. Paths resolve the way VS Code's Markdown preview resolves them:
//! relative to the document, and a `/`-leading path against the workspace
//! root when the document has one. The bytes are then read by the daemon of
//! the host that owns the document, so a remote task's images come from that
//! host rather than this computer's disk.
//!
//! Paths stay strings in the host's own syntax. A Windows desktop renders
//! POSIX paths for SSH and WSL hosts, which `std::path` would misread.

/// What an image URL names.
#[derive(Debug, Eq, PartialEq)]
pub enum ImageTarget<'a> {
    /// An inline `data:` URL, decoded in place.
    Data(&'a str),
    /// A file on the document's host.
    Path(ImagePath),
    /// Network URLs, `~` paths, and relative paths with nothing to resolve
    /// against. These show their caption only.
    Unsupported,
}

/// An absolute image path on the document's host, plus where to look when a
/// `/`-leading path is not under the workspace root after all.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ImagePath {
    pub path: String,
    pub fallback: Option<String>,
}

impl ImagePath {
    fn new(path: String) -> Self {
        Self {
            path,
            fallback: None,
        }
    }
}

/// What relative image paths resolve against.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImageBase {
    /// The document's directory, for `img/a.png`.
    dir: Option<String>,
    /// The workspace root, for `/img/a.png`. Without one such a path is an
    /// ordinary absolute path, which is what agents write in a transcript.
    root: Option<String>,
}

impl ImageBase {
    /// Relative paths resolve against `dir`; `/`-leading ones are absolute.
    pub fn directory(dir: &str) -> Self {
        Self {
            dir: Some(dir.to_owned()),
            root: None,
        }
    }

    /// A file inside a workspace: relative paths resolve against the file's
    /// own directory and `/`-leading ones against the workspace root first.
    pub fn document(root: &str, relative_path: &str) -> Self {
        let parent = relative_path
            .rfind(['/', '\\'])
            .map_or("", |index| &relative_path[..index]);
        Self {
            dir: Some(join(root, parent)),
            root: Some(root.to_owned()),
        }
    }
}

/// Classify one image URL from a Markdown document.
pub fn classify<'a>(url: &'a str, base: &ImageBase) -> ImageTarget<'a> {
    let url = url.trim();
    if has_scheme(url, "data") {
        return ImageTarget::Data(url);
    }
    if has_scheme(url, "file") {
        return file_url_path(url).map_or(ImageTarget::Unsupported, |path| {
            ImageTarget::Path(ImagePath::new(path))
        });
    }
    if is_windows_absolute(url) {
        let path = percent_decode(strip_query(url));
        return ImageTarget::Path(ImagePath::new(normalize_windows(&path)));
    }
    if has_any_scheme(url) || url.starts_with("\\\\") {
        return ImageTarget::Unsupported;
    }

    let path = percent_decode(strip_query(url));
    if path.is_empty() || path.starts_with('~') {
        return ImageTarget::Unsupported;
    }
    if path.starts_with('/') {
        let absolute = normalize_posix(&path);
        return ImageTarget::Path(match base.root.as_deref() {
            // Already a path inside the root: the absolute reading is the
            // only sensible one.
            Some(root) if is_within(&absolute, root) => ImagePath::new(absolute),
            Some(root) if !is_windows_absolute(root) => ImagePath {
                path: join(root, &path),
                fallback: Some(absolute),
            },
            Some(root) => ImagePath::new(join(root, &path)),
            None => ImagePath::new(absolute),
        });
    }
    match base.dir.as_deref() {
        Some(dir) => ImageTarget::Path(ImagePath::new(join(dir, &path))),
        None => ImageTarget::Unsupported,
    }
}

/// Split an absolute host path into its directory and file name.
pub fn split_parent(path: &str) -> Option<(&str, &str)> {
    let separators: &[char] = if is_windows_absolute(path) {
        &['/', '\\']
    } else {
        &['/']
    };
    let index = path.rfind(separators)?;
    let name = &path[index + 1..];
    if name.is_empty() {
        return None;
    }
    let parent = &path[..index];
    // Keep the root's separator: `/a.png` lives in `/`, `C:\a.png` in `C:\`.
    let parent = if parent.is_empty() || parent.ends_with(':') {
        &path[..=index]
    } else {
        parent
    };
    Some((parent, name))
}

fn has_scheme(url: &str, scheme: &str) -> bool {
    url.len() > scheme.len()
        && url.as_bytes()[scheme.len()] == b':'
        && url[..scheme.len()].eq_ignore_ascii_case(scheme)
}

/// `https:`, `mailto:`, and the like. A drive letter (`C:`) is one character
/// long, so it never counts as a scheme.
fn has_any_scheme(url: &str) -> bool {
    let Some(colon) = url.find(':') else {
        return false;
    };
    let scheme = &url[..colon];
    scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

/// The path a `file:` URL names. Only local URLs: `file:///p` and
/// `file://localhost/p`.
fn file_url_path(url: &str) -> Option<String> {
    let rest = url[5..].strip_prefix("//")?;
    let slash = rest.find('/')?;
    let authority = &rest[..slash];
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let path = percent_decode(strip_query(&rest[slash..]));
    // `file:///C:/x` names a Windows path.
    if let Some(windows) = path.strip_prefix('/')
        && is_windows_absolute(windows)
    {
        return Some(normalize_windows(windows));
    }
    Some(normalize_posix(&path))
}

fn strip_query(url: &str) -> &str {
    url.find(['?', '#']).map_or(url, |index| &url[..index])
}

fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) = (
                bytes
                    .get(index + 1)
                    .and_then(|byte| (*byte as char).to_digit(16)),
                bytes
                    .get(index + 2)
                    .and_then(|byte| (*byte as char).to_digit(16)),
            )
        {
            decoded.push((high << 4 | low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `base` joined with a relative `path`, normalized in `base`'s syntax.
fn join(base: &str, path: &str) -> String {
    if is_windows_absolute(base) {
        normalize_windows(&format!("{base}\\{path}"))
    } else {
        normalize_posix(&format!("{base}/{path}"))
    }
}

fn is_within(path: &str, root: &str) -> bool {
    let root = normalize_posix(root);
    root == "/"
        || path
            .strip_prefix(root.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
}

fn normalize_posix(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

fn normalize_windows(path: &str) -> String {
    let (drive, rest) = path.split_at(2);
    let mut parts = Vec::new();
    for part in rest.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("{}\\{}", drive.to_ascii_uppercase(), parts.join("\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> ImageTarget<'static> {
        ImageTarget::Path(ImagePath::new(value.to_owned()))
    }

    #[test]
    fn data_urls_decode_in_place() {
        let url = "data:image/png;base64,aGk=";
        assert_eq!(classify(url, &ImageBase::default()), ImageTarget::Data(url));
    }

    #[test]
    fn network_urls_are_not_loaded() {
        let base = ImageBase::directory("/repo");
        for url in [
            "https://example.com/a.png",
            "http://x/a.png",
            "ftp://x/a.png",
        ] {
            assert_eq!(classify(url, &base), ImageTarget::Unsupported, "{url}");
        }
    }

    #[test]
    fn relative_paths_resolve_against_the_document_directory() {
        let base = ImageBase::directory("/home/me/repo");
        assert_eq!(
            classify("img/a.png", &base),
            path("/home/me/repo/img/a.png")
        );
        assert_eq!(
            classify("./img/../a.png", &base),
            path("/home/me/repo/a.png")
        );
        assert_eq!(
            classify("../b%20c.png?raw=1", &base),
            path("/home/me/b c.png")
        );
    }

    #[test]
    fn relative_paths_need_a_base() {
        assert_eq!(
            classify("a.png", &ImageBase::default()),
            ImageTarget::Unsupported
        );
        assert_eq!(
            classify("~/a.png", &ImageBase::directory("/r")),
            ImageTarget::Unsupported
        );
    }

    /// A transcript has no document root: agents write real absolute paths.
    #[test]
    fn absolute_paths_without_a_root_are_absolute() {
        let base = ImageBase::directory("/repo");
        assert_eq!(classify("/tmp/out.png", &base), path("/tmp/out.png"));
    }

    /// In a document, `/img/a.png` means the workspace root first, the way
    /// GitHub and VS Code read it, and the filesystem path second.
    #[test]
    fn root_relative_paths_try_the_workspace_root_first() {
        let base = ImageBase::document("/home/me/repo", "docs/README.md");
        assert_eq!(
            classify("/img/a.png", &base),
            ImageTarget::Path(ImagePath {
                path: "/home/me/repo/img/a.png".into(),
                fallback: Some("/img/a.png".into()),
            })
        );
        assert_eq!(
            classify("/home/me/repo/img/a.png", &base),
            path("/home/me/repo/img/a.png")
        );
        assert_eq!(
            classify("shot.png", &base),
            path("/home/me/repo/docs/shot.png")
        );
    }

    #[test]
    fn file_urls_name_local_paths() {
        let base = ImageBase::default();
        assert_eq!(
            classify("file:///tmp/a%20b.png", &base),
            path("/tmp/a b.png")
        );
        assert_eq!(
            classify("file://localhost/tmp/a.png", &base),
            path("/tmp/a.png")
        );
        assert_eq!(
            classify("file:///c:/Users/a.png", &base),
            path("C:\\Users\\a.png")
        );
        assert_eq!(
            classify("file://server/a.png", &base),
            ImageTarget::Unsupported
        );
    }

    #[test]
    fn windows_paths_keep_windows_syntax() {
        let base = ImageBase::document("C:\\work\\repo", "docs\\README.md");
        assert_eq!(classify("C:/shots/a.png", &base), path("C:\\shots\\a.png"));
        assert_eq!(
            classify("img/a.png", &base),
            path("C:\\work\\repo\\docs\\img\\a.png")
        );
        assert_eq!(classify("..\\a.png", &base), path("C:\\work\\repo\\a.png"));
        assert_eq!(
            classify("\\\\server\\a.png", &base),
            ImageTarget::Unsupported
        );
    }

    #[test]
    fn split_parent_keeps_root_separators() {
        assert_eq!(split_parent("/a/b.png"), Some(("/a", "b.png")));
        assert_eq!(split_parent("/b.png"), Some(("/", "b.png")));
        assert_eq!(split_parent("C:\\x\\b.png"), Some(("C:\\x", "b.png")));
        assert_eq!(split_parent("C:\\b.png"), Some(("C:\\", "b.png")));
        assert_eq!(split_parent("/a/"), None);
    }
}
