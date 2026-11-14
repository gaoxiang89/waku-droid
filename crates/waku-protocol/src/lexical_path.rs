//! Lexical, I/O-free handling of absolute paths from any host OS.
//!
//! A desktop may talk to a POSIX or Windows daemon, so `std::path::Path`
//! semantics of the local platform are wrong: a `\` is a separator on a
//! Windows remote but a legal file-name byte on a POSIX one. This module
//! parses and compares path strings by the rules of the path's own shape.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root<'a> {
    Posix,
    /// The original two characters, e.g. `"C:"` or `"c:"`.
    Drive(&'a str),
    Unc {
        server: &'a str,
        share: &'a str,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexicalPath<'a> {
    pub root: Root<'a>,
    pub segments: Vec<&'a str>,
}

/// Parse an absolute POSIX, drive, or UNC path. Relative paths, `~`, and
/// drive-relative forms like `C:foo` return `None`.
pub fn parse(raw: &str) -> Option<LexicalPath<'_>> {
    if let Some(tail) = raw.strip_prefix(r"\\?\UNC\") {
        return parse_unc(tail);
    }
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    if !raw.starts_with(r"\\?\")
        && let Some(tail) = trimmed
            .strip_prefix(r"\\")
            .or_else(|| trimmed.strip_prefix("//"))
    {
        return parse_unc(tail);
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || matches!(bytes[2], b'\\' | b'/'))
    {
        return Some(LexicalPath {
            root: Root::Drive(&trimmed[..2]),
            segments: windows_segments(&trimmed[2..]),
        });
    }
    if trimmed.starts_with('/') && !trimmed.starts_with("//") {
        return Some(LexicalPath {
            root: Root::Posix,
            segments: posix_segments(trimmed),
        });
    }
    None
}

fn parse_unc(tail: &str) -> Option<LexicalPath<'_>> {
    let mut parts = windows_segments(tail).into_iter();
    let server = parts.next()?;
    let share = parts.next()?;
    Some(LexicalPath {
        root: Root::Unc { server, share },
        segments: parts.collect(),
    })
}

fn windows_segments(tail: &str) -> Vec<&str> {
    tail.split(['\\', '/'])
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

fn posix_segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

impl<'a> LexicalPath<'a> {
    pub fn is_windows(&self) -> bool {
        matches!(self.root, Root::Drive(_) | Root::Unc { .. })
    }

    pub fn separator(&self) -> char {
        if self.is_windows() { '\\' } else { '/' }
    }

    /// Resolve `..` segments by popping, never above the root.
    pub fn normalized(mut self) -> Self {
        let mut resolved = Vec::with_capacity(self.segments.len());
        for segment in self.segments.drain(..) {
            if segment == ".." {
                resolved.pop();
            } else {
                resolved.push(segment);
            }
        }
        self.segments = resolved;
        self
    }

    /// The path without its last segment; `None` when it has no segments.
    pub fn parent(&self) -> Option<LexicalPath<'a>> {
        let mut segments = self.segments.clone();
        segments.pop()?;
        Some(LexicalPath {
            root: self.root,
            segments,
        })
    }

    /// The remaining segments when `self` sits at or under `base`. Roots must
    /// match and `base`'s segments must equal a prefix of `self`'s; Windows
    /// roots and segments compare ASCII case-insensitively, POSIX exactly.
    pub fn strip_prefix(&self, base: &LexicalPath<'_>) -> Option<&[&'a str]> {
        if !roots_equal(&self.root, &base.root) || self.segments.len() < base.segments.len() {
            return None;
        }
        segments_prefix_equal(
            &self.segments[..base.segments.len()],
            &base.segments,
            self.is_windows(),
        )
        .then(|| &self.segments[base.segments.len()..])
    }

    pub fn starts_with(&self, base: &LexicalPath<'_>) -> bool {
        self.strip_prefix(base).is_some()
    }

    pub fn lexical_eq(&self, other: &LexicalPath<'_>) -> bool {
        roots_equal(&self.root, &other.root)
            && segments_prefix_equal(&self.segments, &other.segments, self.is_windows())
    }

    pub fn to_path_string(&self) -> String {
        match self.root {
            Root::Posix => {
                if self.segments.is_empty() {
                    "/".to_owned()
                } else {
                    format!("/{}", self.segments.join("/"))
                }
            }
            Root::Drive(drive) => {
                if self.segments.is_empty() {
                    format!(r"{drive}\")
                } else {
                    format!(r"{drive}\{}", self.segments.join(r"\"))
                }
            }
            Root::Unc { server, share } => {
                if self.segments.is_empty() {
                    format!(r"\\{server}\{share}")
                } else {
                    format!(r"\\{server}\{share}\{}", self.segments.join(r"\"))
                }
            }
        }
    }
}

fn roots_equal(a: &Root<'_>, b: &Root<'_>) -> bool {
    match (a, b) {
        (Root::Posix, Root::Posix) => true,
        (Root::Drive(a), Root::Drive(b)) => a.eq_ignore_ascii_case(b),
        (
            Root::Unc {
                server: a_server,
                share: a_share,
            },
            Root::Unc {
                server: b_server,
                share: b_share,
            },
        ) => a_server.eq_ignore_ascii_case(b_server) && a_share.eq_ignore_ascii_case(b_share),
        _ => false,
    }
}

fn segments_prefix_equal(a: &[&str], b: &[&str], case_insensitive: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).all(|(a, b)| {
        if case_insensitive {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    })
}

/// Whether `raw` opens with a drive letter, a colon, and a separator
/// (`C:/` or `C:\`). The bare `C:` prefix is deliberately not accepted.
pub fn has_drive_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

/// Whether `raw` is a Windows absolute path: a UNC `\\` prefix or a drive
/// prefix followed by a separator or nothing (`C:` alone counts).
pub fn is_windows_absolute(raw: &str) -> bool {
    if raw.starts_with(r"\\") {
        return true;
    }
    let bytes = raw.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || matches!(bytes[2], b'/' | b'\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> LexicalPath<'_> {
        parse(raw).unwrap_or_else(|| panic!("{raw:?} should parse"))
    }

    #[test]
    fn parses_drive_forms() {
        let p = path(r"C:\Users\me");
        assert_eq!(p.root, Root::Drive("C:"));
        assert_eq!(p.segments, ["Users", "me"]);

        let p = path("c:/users/me");
        assert_eq!(p.root, Root::Drive("c:"));
        assert_eq!(p.segments, ["users", "me"]);

        let p = path("C:");
        assert_eq!(p.root, Root::Drive("C:"));
        assert!(p.segments.is_empty());

        let p = path(r"\\?\C:\x\y");
        assert_eq!(p.root, Root::Drive("C:"));
        assert_eq!(p.segments, ["x", "y"]);

        assert_eq!(path("C:/a\\b").separator(), '\\');
        assert!(path("C:/a").is_windows());
    }

    #[test]
    fn parses_unc_forms() {
        let p = path(r"\\server\share\dir\file");
        assert_eq!(
            p.root,
            Root::Unc {
                server: "server",
                share: "share"
            }
        );
        assert_eq!(p.segments, ["dir", "file"]);

        let p = path("//server/share/dir");
        assert_eq!(
            p.root,
            Root::Unc {
                server: "server",
                share: "share"
            }
        );
        assert_eq!(p.segments, ["dir"]);

        let p = path(r"\\?\UNC\s\sh\x");
        assert_eq!(
            p.root,
            Root::Unc {
                server: "s",
                share: "sh"
            }
        );
        assert_eq!(p.segments, ["x"]);
    }

    #[test]
    fn rejects_unparseable_and_incomplete_roots() {
        for raw in [
            "C:foo",
            "relative/path",
            "~/.waku",
            "",
            r"\\server",
            "//server",
            r"\\?\UNC\onlyserver",
            r"\\?",
        ] {
            assert!(parse(raw).is_none(), "{raw:?} should not parse");
        }
    }

    #[test]
    fn parses_posix_and_keeps_backslashes_in_names() {
        let p = path("/home/me/dir");
        assert_eq!(p.root, Root::Posix);
        assert_eq!(p.segments, ["home", "me", "dir"]);
        assert!(!p.is_windows());
        assert_eq!(p.separator(), '/');

        let p = path(r"/home/odd\name/x");
        assert_eq!(p.segments, ["home", r"odd\name", "x"]);
        assert_eq!(p.to_path_string(), r"/home/odd\name/x");

        let p = path("/");
        assert_eq!(p.root, Root::Posix);
        assert!(p.segments.is_empty());
    }

    #[test]
    fn drops_empty_and_dot_segments_but_keeps_dotdot() {
        let p = path("/a//./b/../c");
        assert_eq!(p.segments, ["a", "b", "..", "c"]);
        let p = path(r"C:\a\.\b\\c");
        assert_eq!(p.segments, ["a", "b", "c"]);
    }

    #[test]
    fn normalized_resolves_dotdot_never_above_root() {
        assert_eq!(path("/a/b/../c").normalized().to_path_string(), "/a/c");
        assert_eq!(path("/a/../..").normalized().to_path_string(), "/");
        assert_eq!(path("/../x").normalized().to_path_string(), "/x");
        assert_eq!(path(r"C:\a\..\..\b").normalized().to_path_string(), r"C:\b");
        assert_eq!(
            path(r"\\s\sh\a\..").normalized().to_path_string(),
            r"\\s\sh"
        );
    }

    #[test]
    fn parent_pops_one_segment_and_none_at_root() {
        assert_eq!(path("/a/b/c").parent().unwrap().to_path_string(), "/a/b");
        assert_eq!(path("/a").parent().unwrap().to_path_string(), "/");
        assert_eq!(path("/").parent(), None);
        assert_eq!(path(r"C:\").parent(), None);
        assert_eq!(path(r"C:\a").parent().unwrap().to_path_string(), "C:\\");
        assert_eq!(path(r"\\s\sh").parent(), None);
        assert_eq!(
            path(r"\\s\sh\x").parent().unwrap().to_path_string(),
            r"\\s\sh"
        );
    }

    #[test]
    fn strip_prefix_matches_windows_case_insensitively() {
        let base = path(r"C:\Users\Me\proj");
        let longer = path(r"c:\users\me\proj\src\app");
        let rest = longer.strip_prefix(&base);
        assert_eq!(rest, Some(["src", "app"].as_slice()));
        assert!(
            path(r"C:\Users\Me\proj")
                .strip_prefix(&base)
                .unwrap()
                .is_empty()
        );
        assert!(path(r"D:\Users\Me\proj\x").strip_prefix(&base).is_none());
        assert!(path(r"C:\Users\Me\pro\x").strip_prefix(&base).is_none());
        assert!(path(r"C:\Users\Me").strip_prefix(&base).is_none());

        let base = path(r"\\srv\share\proj");
        assert_eq!(
            path(r"\\SRV\Share\proj\docs").strip_prefix(&base),
            Some(["docs"].as_slice())
        );
    }

    #[test]
    fn strip_prefix_matches_posix_exactly() {
        let base = path("/home/me/proj");
        assert_eq!(
            path("/home/me/proj/src").strip_prefix(&base),
            Some(["src"].as_slice())
        );
        assert!(path("/home/Me/proj/src").strip_prefix(&base).is_none());
        assert!(path(r"C:\home\me\proj\src").strip_prefix(&base).is_none());
    }

    #[test]
    fn starts_with_and_lexical_eq_follow_root_case_rules() {
        assert!(path(r"c:\a\b").starts_with(&path(r"C:\A")));
        assert!(path("/a/b").starts_with(&path("/a")));
        assert!(!path("/a/b").starts_with(&path("/A")));
        assert!(path(r"c:\a\b").lexical_eq(&path(r"C:\A\b")));
        assert!(path("/a/b").lexical_eq(&path("/a/b")));
        assert!(!path("/a/b").lexical_eq(&path("/a/B")));
        assert!(!path("/a/b").lexical_eq(&path("/a/b/c")));
        assert!(!path(r"C:\a").lexical_eq(&path(r"\\C:\a")));
    }

    #[test]
    fn to_path_string_round_trips() {
        for raw in [
            "/",
            "/a/b",
            "C:\\",
            r"C:\a\b",
            r"\\srv\share",
            r"\\srv\share\a",
        ] {
            assert_eq!(path(raw).to_path_string(), raw);
        }
        assert_eq!(path("c:/a/b").to_path_string(), r"c:\a\b");
        assert_eq!(path("//srv/share/a").to_path_string(), r"\\srv\share\a");
        assert_eq!(path(r"\\?\C:\a").to_path_string(), r"C:\a");
        assert_eq!(path(r"\\?\UNC\s\sh\a").to_path_string(), r"\\s\sh\a");
    }

    #[test]
    fn drive_prefix_predicate_is_strict() {
        assert!(has_drive_prefix(r"C:\Users"));
        assert!(has_drive_prefix("d:/work"));
        assert!(!has_drive_prefix("C:"));
        assert!(!has_drive_prefix("C:foo"));
        assert!(!has_drive_prefix("https://example.com"));
        assert!(!has_drive_prefix("/a/b"));
    }

    #[test]
    fn windows_absolute_predicate_covers_unc_and_bare_drive() {
        assert!(is_windows_absolute(r"C:\a"));
        assert!(is_windows_absolute("c:/a"));
        assert!(is_windows_absolute("C:"));
        assert!(is_windows_absolute(r"\\srv\share"));
        assert!(!is_windows_absolute("C:foo"));
        assert!(!is_windows_absolute("/a/b"));
        assert!(!is_windows_absolute("relative"));
    }
}
