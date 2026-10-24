use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use base64::Engine as _;
use gpui::{KeyBinding, actions};

use super::*;

actions!(
    waku_right_panel,
    [
        CancelRightPanelFileRename,
        ConfirmRightPanelFileDelete,
        DismissRightPanelFileDelete
    ]
);

const FILE_RENAME_PARENT_CONTEXT: &str = "RightPanelFileRename";
const FILE_RENAME_FIELD_CONTEXT: &str = "RightPanelFileRename > TextInput";
const FILE_DELETE_DIALOG_CONTEXT: &str = "RightPanelFileDeleteDialog";

/// Bytes requested per chunk of a streamed download. The daemon clamps to its
/// own cap; this keeps every wire message small and gives the progress card
/// fine-grained updates even for multi-gigabyte files.
const DOWNLOAD_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// Keep Escape inside the focused inline editor so it cancels the rename,
/// and give the delete confirm dialog its confirm/dismiss keys.
pub fn init(cx: &mut App) {
    super::git_graph::init(cx);
    cx.bind_keys([
        KeyBinding::new(
            "escape",
            CancelRightPanelFileRename,
            Some(FILE_RENAME_FIELD_CONTEXT),
        ),
        KeyBinding::new(
            "secondary-enter",
            ConfirmRightPanelFileDelete,
            Some(FILE_DELETE_DIALOG_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            DismissRightPanelFileDelete,
            Some(FILE_DELETE_DIALOG_CONTEXT),
        ),
    ]);
}

pub(super) const TAB_SCROLL_FADE_WIDTH: f32 = 24.0;
const REVIEW_DIFF_FILE_HEADER_HEIGHT: f32 = 36.0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkingTreeEntry {
    relative_path: String,
    absolute_path: PathBuf,
    name: String,
    is_dir: bool,
    file_icon: Option<&'static str>,
    expanded: bool,
    depth: usize,
}

/// The working-tree row an inline rename or create is editing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RightPanelFileRename {
    Rename {
        relative_path: String,
        absolute_path: PathBuf,
    },
    Create {
        parent_relative_path: String,
        depth: usize,
        is_dir: bool,
    },
}

/// A delete parked behind the confirm dialog until the user approves it.
#[derive(Clone, Debug)]
pub(super) struct RightPanelDeleteConfirm {
    pub(super) relative_path: String,
    pub(super) name: String,
    pub(super) is_dir: bool,
    /// Keyboard focus for the safe choice, so Enter cannot confirm by accident.
    pub(super) cancel_focus: FocusHandle,
    pub(super) confirm_focus: FocusHandle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TranscriptLinkRoute {
    ProjectFile(String),
    Finder(PathBuf),
    External,
    /// Recognized as a workspace file, but no project is selected to open it in.
    NoWorkspace,
}

/// Directories that must be expanded so `relative_path` is visible in a
/// working tree that starts collapsed. The file itself is not included.
pub(super) fn ancestor_directories(workspace: &Path, relative_path: &str) -> Vec<PathBuf> {
    if let Some(workspace) = posix_workspace(workspace) {
        let mut directory = workspace.trim_end_matches('/').to_owned();
        let mut ancestors = Vec::new();
        let mut parts = relative_path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            directory.push('/');
            directory.push_str(part);
            ancestors.push(PathBuf::from(&directory));
        }
        return ancestors;
    }
    let mut directory = workspace.to_path_buf();
    let mut ancestors = Vec::new();
    let mut components = Path::new(relative_path).components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        if let std::path::Component::Normal(part) = component {
            directory.push(part);
            ancestors.push(directory.clone());
        }
    }
    ancestors
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode_file_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            )
        {
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| path.to_owned())
}

fn markdown_file_link_path(target: &str) -> Option<PathBuf> {
    let target = crate::md::parser::strip_file_location(target.trim());
    if target
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
    {
        return url::Url::parse(target).ok()?.to_file_path().ok();
    }

    let path = PathBuf::from(percent_decode_file_path(target));
    path.is_absolute().then_some(path)
}

fn normalized_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn workspace_relative_file_path(workspace: &Path, target: &Path) -> Option<String> {
    fn relative(workspace: &Path, target: &Path) -> Option<String> {
        let relative = target.strip_prefix(workspace).ok()?;
        if relative.as_os_str().is_empty() {
            return None;
        }
        Some(relative.to_string_lossy().into_owned())
    }

    let workspace = normalized_path(workspace);
    let target = normalized_path(target);
    // These are daemon-host paths. Routing is intentionally lexical: probing
    // the desktop filesystem would reinterpret a remote workspace locally.
    relative(&workspace, &target)
}

/// A workspace path in POSIX form, as SSH and WSL hosts report it.
///
/// A Windows desktop's `std::path` reads `/home/me` as relative and joins
/// with `\`, which a Linux daemon takes as part of a file name, so paths on
/// these hosts are routed as strings instead.
fn posix_workspace(workspace: &Path) -> Option<&str> {
    workspace.to_str().filter(|path| path.starts_with('/'))
}

fn normalized_posix_path(path: &str) -> String {
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

/// The path after `~/` (or `~\`) when `text` names a user's home. Such
/// references must be expanded against a home directory, never joined onto
/// the workspace.
fn home_relative_path(text: &str) -> Option<&str> {
    text.strip_prefix("~/")
        .or_else(|| text.strip_prefix("~\\"))
        .filter(|rest| !rest.is_empty())
}

/// Home directory of a POSIX daemon host, derived from a registered
/// projectless root on that host.
fn daemon_posix_home() -> Option<PathBuf> {
    crate::projectless::workspace_roots()
        .into_iter()
        .find(|root| root.to_string_lossy().starts_with('/'))
        .and_then(|root| root.parent().and_then(Path::parent).map(Path::to_path_buf))
}

fn posix_workspace_relative_file_path(workspace: &str, target: &str) -> Option<String> {
    let workspace = normalized_posix_path(workspace);
    let relative = if workspace == "/" {
        target.strip_prefix('/')
    } else {
        target.strip_prefix(workspace.as_str())?.strip_prefix('/')
    }?;
    (!relative.is_empty()).then(|| relative.to_owned())
}

fn posix_transcript_link_route(target: &str, workspace: &str) -> TranscriptLinkRoute {
    let trimmed = crate::md::parser::strip_file_location(target.trim());
    let path = if trimmed
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
    {
        let Some(url) = url::Url::parse(trimmed)
            .ok()
            .filter(|url| matches!(url.host_str(), None | Some("" | "localhost")))
        else {
            return TranscriptLinkRoute::External;
        };
        percent_decode_file_path(url.path())
    } else if trimmed.starts_with('/') {
        percent_decode_file_path(trimmed)
    } else {
        let Some(reference) = crate::md::parser::file_reference_link(target) else {
            return TranscriptLinkRoute::External;
        };
        let relative = percent_decode_file_path(crate::md::parser::strip_file_location(reference));
        if relative.is_empty() {
            return TranscriptLinkRoute::NoWorkspace;
        }
        // `~/x` names the daemon user's home. Joining it under the workspace
        // would open a nonexistent `<workspace>/~/x`; expand it against the
        // home derived from a registered projectless root instead.
        if let Some(rest) = home_relative_path(&relative) {
            return match daemon_posix_home() {
                Some(home) => {
                    let home = home.to_string_lossy().trim_end_matches('/').to_owned();
                    let path = normalized_posix_path(&format!("{home}/{rest}"));
                    match posix_workspace_relative_file_path(workspace, &path) {
                        Some(relative_path) => TranscriptLinkRoute::ProjectFile(relative_path),
                        None => TranscriptLinkRoute::Finder(PathBuf::from(path)),
                    }
                }
                // The daemon's home is unknown; admit it rather than open a
                // bogus workspace-relative path.
                None => TranscriptLinkRoute::Finder(PathBuf::from(relative)),
            };
        }
        format!("{workspace}/{relative}")
    };
    let path = normalized_posix_path(&path);
    match posix_workspace_relative_file_path(workspace, &path) {
        Some(relative_path) => TranscriptLinkRoute::ProjectFile(relative_path),
        None => TranscriptLinkRoute::Finder(PathBuf::from(path)),
    }
}

fn transcript_link_route(target: &str, workspace: Option<&Path>) -> TranscriptLinkRoute {
    if let Some(workspace) = workspace.and_then(posix_workspace) {
        return posix_transcript_link_route(target, workspace);
    }
    if let Some(path) = markdown_file_link_path(target) {
        let path = normalized_path(&path);
        return if let Some(relative_path) =
            workspace.and_then(|workspace| workspace_relative_file_path(workspace, &path))
        {
            TranscriptLinkRoute::ProjectFile(relative_path)
        } else {
            TranscriptLinkRoute::Finder(path)
        };
    }

    let Some(reference) = crate::md::parser::file_reference_link(target) else {
        return TranscriptLinkRoute::External;
    };
    let stripped = percent_decode_file_path(crate::md::parser::strip_file_location(reference));
    // `~/x` names the user's home, not a workspace-relative path; joining it
    // onto the workspace opens a nonexistent `<workspace>/~/x`.
    if let Some(rest) = home_relative_path(&stripped)
        && let Some(home) = dirs::home_dir()
    {
        let joined = normalized_path(&home.join(rest));
        return if let Some(relative_path) =
            workspace.and_then(|workspace| workspace_relative_file_path(workspace, &joined))
        {
            TranscriptLinkRoute::ProjectFile(relative_path)
        } else {
            TranscriptLinkRoute::Finder(joined)
        };
    }
    let Some(workspace) = workspace else {
        return TranscriptLinkRoute::NoWorkspace;
    };
    let relative = PathBuf::from(stripped);
    if relative.as_os_str().is_empty() {
        return TranscriptLinkRoute::NoWorkspace;
    }
    let joined = normalized_path(&workspace.join(relative));
    if let Some(relative_path) = workspace_relative_file_path(workspace, &joined) {
        TranscriptLinkRoute::ProjectFile(relative_path)
    } else {
        TranscriptLinkRoute::Finder(joined)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ReviewDiffTreeRow {
    Directory {
        path: String,
        name: String,
        depth: usize,
        expanded: bool,
    },
    File {
        file_index: usize,
        depth: usize,
    },
}

/// Select from a compact, embedded subset of Material Icon Theme rather than
/// shipping its entire icon catalog. The SVG path is resolved once per entry
/// during the directory scan, not on every row paint.
pub(super) fn file_icon_for_path(path: &str) -> &'static str {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    file_icon_for_name(name)
}

fn review_diff_gap_icon_path(direction: crate::review_diff::ExpansionDirection) -> &'static str {
    match direction {
        // Pierre's direction attributes and rendered chevrons are inverted by
        // CSS. Waku names the data operation directly, so encode the resulting
        // visual here: reveal-from-start points down; reveal-from-end points up.
        crate::review_diff::ExpansionDirection::Start => "icons/chevron-down.svg",
        crate::review_diff::ExpansionDirection::End => "icons/chevron-up.svg",
        crate::review_diff::ExpansionDirection::Both
        | crate::review_diff::ExpansionDirection::All => "icons/chevrons-up-down.svg",
    }
}

fn review_diff_gap_tooltip(direction: crate::review_diff::ExpansionDirection) -> String {
    match direction {
        crate::review_diff::ExpansionDirection::Start => tr!("diff.expand_context_below"),
        crate::review_diff::ExpansionDirection::End => tr!("diff.expand_context_above"),
        crate::review_diff::ExpansionDirection::Both => tr!("diff.expand_context"),
        crate::review_diff::ExpansionDirection::All => tr!("diff.expand_all_context"),
    }
}

fn review_diff_gap_directions(
    position: crate::review_diff::GapPosition,
    chunked: bool,
) -> &'static [crate::review_diff::ExpansionDirection] {
    use crate::review_diff::{ExpansionDirection, GapPosition};

    match (position, chunked) {
        (GapPosition::Leading, _) => &[ExpansionDirection::End],
        (GapPosition::Trailing, _) => &[ExpansionDirection::Start],
        (GapPosition::Between, false) => &[ExpansionDirection::Both],
        (GapPosition::Between, true) => &[ExpansionDirection::Start, ExpansionDirection::End],
    }
}

fn review_diff_directory_paths(files: &[crate::review_diff::File]) -> HashSet<String> {
    let mut paths = HashSet::new();
    for file in files {
        let parts = file.path.split('/').collect::<Vec<_>>();
        let mut path = String::new();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(part);
            paths.insert(path.clone());
        }
    }
    paths
}

fn review_diff_tree_rows(
    files: &[crate::review_diff::File],
    expanded_paths: &HashSet<String>,
    filter: &str,
) -> Vec<ReviewDiffTreeRow> {
    let filter = filter.trim().to_ascii_lowercase();
    let filtering = !filter.is_empty();
    let mut indexes = files
        .iter()
        .enumerate()
        .filter(|(_, file)| {
            filtering
                .then(|| file.path.to_ascii_lowercase().contains(&filter))
                .unwrap_or(true)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    indexes.sort_by_key(|index| files[*index].path.to_ascii_lowercase());

    let mut rows = Vec::new();
    let mut emitted_directories = HashSet::new();
    for file_index in indexes {
        let parts = files[file_index].path.split('/').collect::<Vec<_>>();
        let mut directory = String::new();
        let mut visible = true;
        for (depth, part) in parts.iter().take(parts.len().saturating_sub(1)).enumerate() {
            if !directory.is_empty() {
                directory.push('/');
            }
            directory.push_str(part);
            let expanded = filtering || expanded_paths.contains(&directory);
            if emitted_directories.insert(directory.clone()) && visible {
                rows.push(ReviewDiffTreeRow::Directory {
                    path: directory.clone(),
                    name: (*part).to_owned(),
                    depth,
                    expanded,
                });
            }
            if !expanded {
                visible = false;
                break;
            }
        }
        if visible {
            rows.push(ReviewDiffTreeRow::File {
                file_index,
                depth: parts.len().saturating_sub(1),
            });
        }
    }
    rows
}

/// How wide and tall a diff row is drawn. The Review panel is a reading
/// surface; the copy embedded in a transcript activity is a summary and gives
/// its space back to the code.
#[derive(Clone, Copy)]
pub(super) struct DiffRowStyle {
    gutter_width: f32,
    row_height: f32,
    text_size: f32,
    /// What to put in the gutter of a row that has no line number. Git always
    /// reports positions, so this only comes up on a diff synthesized from a
    /// provider's before/after text: there the `+`/`-` marker stands in, which
    /// keeps the gutter from going blank and the meaning off color alone.
    marker_fallback: bool,
}

impl DiffRowStyle {
    /// Review-tab rows at the user's code font size. The gutter holds a
    /// right-aligned line number: ~0.6em per mono digit, five digits, plus
    /// its padding and border.
    pub(super) fn review(text_size: f32) -> Self {
        Self {
            gutter_width: (text_size * 3.0 + 14.0).round(),
            row_height: (text_size * 1.5).round(),
            text_size,
            marker_fallback: false,
        }
    }

    /// The same rows the Review tab draws, so an edit reads the same wherever
    /// it is opened.
    pub(super) fn activity(text_size: f32) -> Self {
        Self {
            marker_fallback: true,
            ..Self::review(text_size)
        }
    }

    pub(super) fn gutter_width(&self) -> f32 {
        self.gutter_width
    }
}

/// Selection identity for one diff code row. Selection resolves a drag by
/// looking rows up by key, so every row must have its own.
///
/// Rows with line numbers key on them: they survive Review's gap expansion,
/// where a revealed gap shifts every later row's index. Rows without them — a
/// diff synthesized from a provider's before/after text — key on the row index
/// instead, which is stable there because an activity diff is only ever
/// rebuilt whole. Keying those on their (absent) numbers gave every added row
/// the same key, and a drag resolved against whichever duplicate registered
/// first: selections jumped rows, skipped wrapped lines, and collapsed when
/// the head crossed into context.
fn diff_row_selection_key(
    key_prefix: &str,
    line: &crate::review_diff::Line,
    index: usize,
) -> String {
    let kind = match &line.kind {
        crate::review_diff::LineKind::Context => "context",
        crate::review_diff::LineKind::Addition => "addition",
        crate::review_diff::LineKind::Deletion => "deletion",
        _ => "other",
    };
    match (line.old_line, line.new_line) {
        (None, None) => format!("{key_prefix}-line-{}-{kind}-i{index}", line.file_index),
        (old, new) => format!(
            "{key_prefix}-line-{}-{kind}-{}-{}",
            line.file_index,
            old.unwrap_or(0),
            new.unwrap_or(0),
        ),
    }
}

/// One context, addition, or deletion row, shared by the Review panel and the
/// diff inside an expanded file-change activity so the two never drift.
pub(super) fn render_diff_code_row(
    line: &crate::review_diff::Line,
    index: usize,
    key_prefix: &str,
    selection: &TranscriptSelection,
    style: DiffRowStyle,
    theme: &Theme,
) -> AnyElement {
    let semantic_body_opacity = if theme.is_dark { 0.20 } else { 0.12 };
    let semantic_gutter_opacity = if theme.is_dark { 0.15 } else { 0.09 };
    let (marker, body_background, gutter_background, edge, number_color) = match &line.kind {
        crate::review_diff::LineKind::Addition => (
            "+",
            Some(theme.success.opacity(semantic_body_opacity)),
            Some(theme.success.opacity(semantic_gutter_opacity)),
            Some(theme.success),
            theme.success,
        ),
        crate::review_diff::LineKind::Deletion => (
            "-",
            Some(theme.danger.opacity(semantic_body_opacity)),
            Some(theme.danger.opacity(semantic_gutter_opacity)),
            Some(theme.danger),
            theme.danger,
        ),
        _ => (" ", None, None, None, theme.text_tertiary),
    };
    let shown_line = line.new_line.or(line.old_line);
    let flat = review_diff_flat_text(line, theme);
    let selectable = md::render::selectable_flat_text(
        &flat,
        crate::md::selection::TextKey::new(diff_row_selection_key(key_prefix, line, index), 0),
        selection.clone(),
        theme.code_wash,
        theme.selection,
        false,
    );
    let gutter = div()
        .w(px(style.gutter_width))
        .min_h(px(style.row_height))
        .self_stretch()
        .flex_none()
        .pr(px(9.0))
        .flex()
        .items_start()
        .justify_end()
        .border_r_1()
        .border_color(theme.border)
        .text_color(number_color)
        .when_some(gutter_background, |gutter, background| {
            gutter.bg(background)
        })
        .child(
            shown_line
                .map(|line| line.to_string())
                .or_else(|| style.marker_fallback.then(|| marker.to_owned()))
                .unwrap_or_default(),
        );
    let body = div()
        .min_h(px(style.row_height))
        .self_stretch()
        .min_w_0()
        .flex_1()
        .pl(px(12.0))
        .flex()
        .items_start()
        .when_some(body_background, |body, background| body.bg(background))
        .child(
            div()
                .id(SharedString::from(format!(
                    "{key_prefix}-line-content-{index}"
                )))
                .min_h(px(style.row_height))
                .min_w_0()
                .flex_1()
                .pr(px(10.0))
                .flex()
                .items_start()
                .overflow_hidden()
                .whitespace_normal()
                .child(selectable),
        );
    div()
        .id(SharedString::from(format!("{key_prefix}-row-{index}")))
        .w_full()
        .min_w_0()
        .min_h(px(style.row_height))
        // A wrapped line makes the row taller than one line. Stacked in a
        // scrolling column, a shrinkable row would be squeezed back to one
        // and paint its overflow over the row beneath it.
        .flex_none()
        .flex()
        .items_stretch()
        .font_family(md::render::MONO_FAMILY)
        .text_size(px(style.text_size))
        .line_height(px(style.row_height))
        .when_some(edge, |row, edge| row.border_l_2().border_color(edge))
        .child(gutter)
        .child(body)
        .into_any_element()
}

fn review_diff_flat_text(line: &crate::review_diff::Line, theme: &Theme) -> md::render::FlatText {
    let text = line.content.clone();
    let palette = MarkdownPalette::from_theme(theme);
    let code_font = font(md::render::MONO_FAMILY);
    let mut runs = Vec::with_capacity(line.tokens.len() * 2 + 1);
    let mut offset = 0;
    let mut push = |len: usize, color: Hsla| {
        if len > 0 {
            runs.push(TextRun {
                len,
                font: code_font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
    };
    for token in &line.tokens {
        if token.range.start > offset {
            push(token.range.start - offset, theme.text_secondary);
        }
        push(token.range.len(), palette.token(token.class));
        offset = token.range.end;
    }
    if offset < text.len() {
        push(text.len() - offset, theme.text_secondary);
    }
    md::render::FlatText {
        text: text.into(),
        runs,
        links: Vec::new(),
        code_ranges: Vec::new(),
        math: None,
    }
}

fn file_icon_for_name(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    let named_icon = if name.starts_with("readme") {
        Some("icons/file-types/readme.svg")
    } else if name.starts_with("license")
        || name.starts_with("licence")
        || name.starts_with("copying")
    {
        Some("icons/file-types/certificate.svg")
    } else if name.starts_with("dockerfile") || name.starts_with("compose.") {
        Some("icons/file-types/docker.svg")
    } else if name == "cmakelists.txt" || name.starts_with("cmake.") {
        Some("icons/file-types/cmake.svg")
    } else if name == "makefile" || name.starts_with("makefile.") || name == "justfile" {
        Some("icons/file-types/makefile.svg")
    } else if matches!(
        name.as_str(),
        "cargo.toml" | "cargo.lock" | "rust-toolchain.toml"
    ) {
        Some("icons/file-types/rust.svg")
    } else if matches!(name.as_str(), "go.mod" | "go.sum" | "go.work") {
        Some("icons/file-types/go.svg")
    } else if name == "pyproject.toml" || name == "pipfile" || name.starts_with("requirements") {
        Some("icons/file-types/python.svg")
    } else if matches!(name.as_str(), "bun.lock" | "bun.lockb" | "bunfig.toml") {
        Some("icons/file-types/bun.svg")
    } else if name.starts_with("pnpm-") || name == ".pnpmfile.cjs" {
        Some("icons/file-types/pnpm.svg")
    } else if name == "yarn.lock" || name.starts_with(".yarnrc") {
        Some("icons/file-types/yarn.svg")
    } else if name == "package.json" {
        Some("icons/file-types/nodejs.svg")
    } else if name == "package-lock.json" {
        Some("icons/file-types/npm.svg")
    } else if name.starts_with("tsconfig.") || name == "tsconfig.json" {
        Some("icons/file-types/typescript.svg")
    } else if name.starts_with("jsconfig.") || name == "jsconfig.json" {
        Some("icons/file-types/javascript.svg")
    } else if name == ".gitignore"
        || name == ".gitattributes"
        || name == ".gitmodules"
        || name == ".gitconfig"
    {
        Some("icons/file-types/git.svg")
    } else if name == ".editorconfig" {
        Some("icons/file-types/editorconfig.svg")
    } else if name.starts_with(".env") {
        Some("icons/file-types/settings.svg")
    } else if name.starts_with(".prettier") || name.starts_with("prettier.config.") {
        Some("icons/file-types/prettier.svg")
    } else if name.starts_with(".eslint") || name.starts_with("eslint.config.") {
        Some("icons/file-types/eslint.svg")
    } else if name.starts_with("biome.json") {
        Some("icons/file-types/biome.svg")
    } else if name.starts_with(".babel") || name.starts_with("babel.config.") {
        Some("icons/file-types/babel.svg")
    } else if name.starts_with(".stylelint") || name.starts_with("stylelint.config.") {
        Some("icons/file-types/stylelint.svg")
    } else if name.starts_with("vite.config.") {
        Some("icons/file-types/vite.svg")
    } else if name.starts_with("vitest.config.") || name.starts_with("vitest.workspace.") {
        Some("icons/file-types/vitest.svg")
    } else if name.starts_with("webpack.") {
        Some("icons/file-types/webpack.svg")
    } else if name.starts_with("rollup.config.") {
        Some("icons/file-types/rollup.svg")
    } else if name.starts_with("next.config.") {
        Some("icons/file-types/next.svg")
    } else if name == "next-env.d.ts" {
        Some("icons/file-types/next.svg")
    } else if name.starts_with("nuxt.config.") || name == ".nuxtrc" {
        Some("icons/file-types/nuxt.svg")
    } else if name.starts_with("astro.config.") {
        Some("icons/file-types/astro.svg")
    } else if name == "angular.json" || name.ends_with(".component.ts") {
        Some("icons/file-types/angular.svg")
    } else if name == "nest-cli.json" {
        Some("icons/file-types/nest.svg")
    } else if name.starts_with("tailwind.config.") {
        Some("icons/file-types/tailwindcss.svg")
    } else if name.starts_with("svelte.config.") {
        Some("icons/file-types/svelte.svg")
    } else if name.starts_with("vue.config.") {
        Some("icons/file-types/vue.svg")
    } else if name == "firebase.json" || name == ".firebaserc" {
        Some("icons/file-types/firebase.svg")
    } else if name == "supabase.toml" {
        Some("icons/file-types/supabase.svg")
    } else if name.starts_with("prisma.config.") {
        Some("icons/file-types/prisma.svg")
    } else if name == "turbo.json" {
        Some("icons/file-types/turborepo.svg")
    } else if name.starts_with("deno.json") || name == "deno.lock" {
        Some("icons/file-types/deno.svg")
    } else if name == ".gitlab-ci.yml" || name == ".gitlab-ci.yaml" {
        Some("icons/file-types/gitlab.svg")
    } else if name == "kustomization.yaml" || name == "kustomization.yml" {
        Some("icons/file-types/kubernetes.svg")
    } else if name == "chart.yaml" || name == "values.yaml" {
        Some("icons/file-types/helm.svg")
    } else if name == "nginx.conf" {
        Some("icons/file-types/nginx.svg")
    } else if name == ".nvmrc" || name == ".node-version" {
        Some("icons/file-types/nodejs.svg")
    } else if name == "build.gradle"
        || name == "settings.gradle"
        || name == "gradlew"
        || name == "gradlew.bat"
    {
        Some("icons/file-types/gradle.svg")
    } else if name.contains(".stories.") || name.contains(".story.") {
        Some("icons/file-types/storybook.svg")
    } else if name == "gemfile" || name == "gemfile.lock" {
        Some("icons/file-types/ruby.svg")
    } else if name == "pom.xml" {
        Some("icons/file-types/java.svg")
    } else {
        None
    };
    if let Some(icon) = named_icon {
        return icon;
    }

    let extension = Path::new(&name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    match extension {
        "rs" => "icons/file-types/rust.svg",
        "js" | "mjs" | "cjs" => "icons/file-types/javascript.svg",
        "ts" | "mts" | "cts" => "icons/file-types/typescript.svg",
        "jsx" | "tsx" => "icons/file-types/react.svg",
        "py" | "pyi" | "pyw" => "icons/file-types/python.svg",
        "go" => "icons/file-types/go.svg",
        "c" | "h" | "m" => "icons/file-types/c.svg",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" | "mm" => "icons/file-types/cpp.svg",
        "cs" => "icons/file-types/csharp.svg",
        "swift" => "icons/file-types/swift.svg",
        "kt" | "kts" => "icons/file-types/kotlin.svg",
        "java" | "class" => "icons/file-types/java.svg",
        "rb" => "icons/file-types/ruby.svg",
        "php" => "icons/file-types/php.svg",
        "html" | "htm" => "icons/file-types/html.svg",
        "css" | "less" => "icons/file-types/css.svg",
        "scss" | "sass" => "icons/file-types/sass.svg",
        "json" | "jsonc" | "jsonl" => "icons/file-types/json.svg",
        "yaml" | "yml" => "icons/file-types/yaml.svg",
        "toml" | "ini" | "cfg" | "conf" | "config" => "icons/file-types/settings.svg",
        "xml" | "xsl" | "plist" => "icons/file-types/xml.svg",
        "md" | "mdx" | "markdown" => "icons/file-types/markdown.svg",
        "sh" | "bash" | "zsh" | "fish" => "icons/file-types/console.svg",
        "ps1" | "psm1" => "icons/file-types/powershell.svg",
        "sql" | "db" | "sqlite" | "sqlite3" | "csv" | "xls" | "xlsx" => {
            "icons/file-types/database.svg"
        }
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "ico" | "tiff" => {
            "icons/file-types/image.svg"
        }
        "svg" => "icons/file-types/svg.svg",
        "pdf" => "icons/file-types/pdf.svg",
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "icons/file-types/audio.svg",
        "mp4" | "mov" | "avi" | "webm" | "mkv" => "icons/file-types/video.svg",
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "tar" | "jar" => {
            "icons/file-types/zip.svg"
        }
        "wasm" | "wat" => "icons/file-types/webassembly.svg",
        "svelte" => "icons/file-types/svelte.svg",
        "vue" => "icons/file-types/vue.svg",
        "tf" | "tfvars" => "icons/file-types/terraform.svg",
        "graphql" | "gql" => "icons/file-types/graphql.svg",
        "lua" => "icons/file-types/lua.svg",
        "dart" => "icons/file-types/dart.svg",
        "astro" => "icons/file-types/astro.svg",
        "coffee" | "cson" => "icons/file-types/coffee.svg",
        "cr" => "icons/file-types/crystal.svg",
        "ex" | "exs" => "icons/file-types/elixir.svg",
        "elm" => "icons/file-types/elm.svg",
        "erl" | "hrl" => "icons/file-types/erlang.svg",
        "clj" | "cljs" | "cljc" | "edn" => "icons/file-types/clojure.svg",
        "hs" | "lhs" => "icons/file-types/haskell.svg",
        "hx" | "hxml" => "icons/file-types/haxe.svg",
        "jinja" | "jinja2" | "j2" => "icons/file-types/jinja.svg",
        "jl" => "icons/file-types/julia.svg",
        "ml" | "mli" => "icons/file-types/ocaml.svg",
        "pl" | "pm" => "icons/file-types/perl.svg",
        "prisma" => "icons/file-types/prisma.svg",
        "pug" | "jade" => "icons/file-types/pug.svg",
        "scala" | "sbt" | "sc" => "icons/file-types/scala.svg",
        "sol" => "icons/file-types/solidity.svg",
        "tex" | "sty" | "cls" => "icons/file-types/tex.svg",
        "xaml" => "icons/file-types/xaml.svg",
        "zig" => "icons/file-types/zig.svg",
        "nix" => "icons/file-types/nix.svg",
        "proto" => "icons/file-types/proto.svg",
        "diff" | "patch" => "icons/file-types/diff.svg",
        "exe" | "dll" | "so" | "dylib" => "icons/file-types/exe.svg",
        "lock" => "icons/file-types/lock.svg",
        _ => "icons/file-types/file.svg",
    }
}

#[cfg(test)]
fn visible_working_tree_entries(
    root: &Path,
    expanded_paths: &HashSet<PathBuf>,
) -> Vec<WorkingTreeEntry> {
    fn visit(
        directory: &Path,
        relative_directory: &Path,
        depth: usize,
        expanded_paths: &HashSet<PathBuf>,
        entries: &mut Vec<WorkingTreeEntry>,
    ) {
        let Ok(read_dir) = std::fs::read_dir(directory) else {
            return;
        };
        let mut children = read_dir
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    return None;
                }
                let is_dir = entry.file_type().ok()?.is_dir();
                Some((entry.path(), name, is_dir))
            })
            .collect::<Vec<_>>();
        children.sort_by_key(|(_, name, is_dir)| (!*is_dir, name.to_lowercase()));

        for (absolute_path, name, is_dir) in children {
            let relative_path = relative_directory.join(&name);
            let expanded = is_dir && expanded_paths.contains(&absolute_path);
            let file_icon = (!is_dir).then(|| file_icon_for_name(&name));
            entries.push(WorkingTreeEntry {
                relative_path: relative_path.to_string_lossy().into_owned(),
                absolute_path: absolute_path.clone(),
                name,
                is_dir,
                file_icon,
                expanded,
                depth,
            });
            if expanded {
                visit(
                    &absolute_path,
                    &relative_path,
                    depth + 1,
                    expanded_paths,
                    entries,
                );
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, Path::new(""), 0, expanded_paths, &mut entries);
    entries
}

/// The language name for a file, as understood by [`crate::md::highlight`].
/// Names the lexer does not know simply render unhighlighted.
fn file_highlighter_language(relative_path: &str) -> &'static str {
    let path = Path::new(relative_path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let normalized_file_name = file_name.to_ascii_lowercase();

    // Lockfiles often have a generic `.lock` suffix (or no useful extension),
    // so resolve their actual serialization format before extension fallback.
    let lockfile_language = match normalized_file_name.as_str() {
        "bun.lock"
        | "composer.lock"
        | "conan.lock"
        | "deno.lock"
        | "flake.lock"
        | "npm-shrinkwrap.json"
        | "package-lock.json"
        | "package.resolved"
        | "packages.lock.json"
        | "pipfile.lock" => Some("json"),
        "cargo.lock" | "pdm.lock" | "poetry.lock" | "uv.lock" => Some("toml"),
        "chart.lock" | "gemfile.lock" | "pnpm-lock.yaml" | "podfile.lock" | "pubspec.lock"
        | "yarn.lock" => Some("yaml"),
        "mix.lock" => Some("elixir"),
        _ => None,
    };
    if let Some(language) = lockfile_language {
        return language;
    }

    if file_name == "Makefile" || file_name.starts_with("Makefile.") {
        return "make";
    }
    if normalized_file_name == "dockerfile" || normalized_file_name.starts_with("dockerfile.") {
        return "dockerfile";
    }

    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("rs") => "rust",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "tsx",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("py" | "pyi") => "python",
        Some("go") => "go",
        Some("c") => "c",
        Some("h" | "hpp" | "hh" | "hxx" | "cc" | "cpp" | "cxx") => "cpp",
        Some("m" | "mm") => "objc",
        Some("java" | "kt" | "kts") => "java",
        Some("cs") => "csharp",
        Some("scala" | "sc") => "scala",
        Some("rb" | "rake" | "gemspec") => "ruby",
        Some("swift") => "swift",
        Some("json" | "jsonc" | "json5") => "json",
        Some("yaml" | "yml") => "yaml",
        Some("toml") => "toml",
        Some("ini" | "cfg" | "conf") => "ini",
        Some("sh" | "bash" | "zsh" | "fish") => "bash",
        Some("css" | "scss" | "sass" | "less") => "css",
        Some("html" | "htm" | "xml" | "svg" | "vue" | "svelte") => "html",
        Some("sql") => "sql",
        Some("diff" | "patch") => "diff",
        Some("md" | "markdown" | "mdx") => "markdown",
        _ => "text",
    }
}

/// Reads a file for the editor, returning its text and whether it can be saved.
///
/// One unbounded `read_to_string`, so callers keep it off the UI thread; the
/// only caller is [`Waku::read_right_panel_file_into_editor`].
fn read_right_panel_file(
    workspace: &waku_client::WorkspaceClient,
    project_path: &Path,
    relative_path: &str,
) -> (String, bool) {
    match workspace.request(waku_client::WorkspaceOperation::ReadTextFile {
        root: project_path.to_path_buf(),
        relative_path: PathBuf::from(relative_path),
    }) {
        Ok(waku_client::WorkspaceResult::TextFile { content }) => (content, true),
        Ok(_) => (
            tr!(
                "files.unable_to_edit",
                error = "the daemon returned an invalid file response"
            ),
            false,
        ),
        Err(error) => (
            tr!("files.unable_to_edit", error = error.to_string()),
            false,
        ),
    }
}

impl RightPanelSurface {
    fn new_browser() -> Self {
        Self::Browser(Uuid::new_v4())
    }

    pub(super) fn new_terminal() -> Self {
        Self::Terminal(Uuid::new_v4())
    }

    fn terminal_id(&self) -> Option<Uuid> {
        match self {
            Self::Terminal(id) => Some(*id),
            _ => None,
        }
    }

    fn browser_id(&self) -> Option<Uuid> {
        match self {
            Self::Browser(id) => Some(*id),
            _ => None,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Browser(_) => tr!("right_panel.browser"),
            Self::Terminal(_) => tr!("right_panel.terminal"),
            Self::BackgroundWork { key, title } => {
                if title.is_empty() {
                    match key.kind {
                        BackgroundWorkKind::Process => tr!("background.process"),
                        BackgroundWorkKind::Monitor => tr!("background.monitor"),
                        BackgroundWorkKind::Subagent => tr!("background.subagent"),
                    }
                } else {
                    title.clone()
                }
            }
            Self::Files => tr!("right_panel.files"),
            Self::Diff => tr!("right_panel.diff"),
            Self::GitGraph => tr!("right_panel.git_graph"),
            Self::File(path) => path.rsplit('/').next().unwrap_or(path).to_owned(),
        }
    }

    fn icon_path(&self) -> &'static str {
        match self {
            Self::Browser(_) => "icons/globe.svg",
            Self::Terminal(_) => "icons/terminal.svg",
            Self::BackgroundWork { key, .. } => work_kind_icon(key.kind),
            Self::Files => "icons/folder.svg",
            Self::Diff => "icons/file-diff.svg",
            Self::GitGraph => "icons/git-commit-horizontal.svg",
            Self::File(path) => file_icon_for_path(path),
        }
    }
}

fn right_panel_tab_label(surface: &RightPanelSurface, files_selected_path: Option<&str>) -> String {
    let label = match surface {
        RightPanelSurface::Files => files_selected_path
            .and_then(|path| Path::new(path).file_name())
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| tr!("right_panel.files")),
        _ => surface.label(),
    };
    single_line_label(&label)
}

fn right_panel_tab_icon(
    surface: &RightPanelSurface,
    files_selected_path: Option<&str>,
) -> &'static str {
    match surface {
        RightPanelSurface::Files => files_selected_path
            .map(file_icon_for_path)
            .unwrap_or_else(|| surface.icon_path()),
        _ => surface.icon_path(),
    }
}

/// What the header's right-panel terminal button should do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RightPanelTerminalToggle {
    /// A terminal is already the surface on screen.
    Hide,
    /// Reveal this existing terminal tab.
    Show(usize),
    /// No terminal tab yet.
    OpenNew,
}

/// Files and Git are single-instance surfaces. A terminal tab is not, so the
/// header button hides the panel only while a terminal is showing, and
/// otherwise reveals the active terminal tab, the first one, or a new one.
fn right_panel_terminal_toggle(
    visible: bool,
    surfaces: &[RightPanelSurface],
    active: Option<usize>,
) -> RightPanelTerminalToggle {
    let active_terminal = active.filter(|index| {
        surfaces
            .get(*index)
            .is_some_and(|surface| matches!(surface, RightPanelSurface::Terminal(_)))
    });
    if visible && active_terminal.is_some() {
        return RightPanelTerminalToggle::Hide;
    }
    if let Some(index) = active_terminal.or_else(|| {
        surfaces
            .iter()
            .position(|surface| matches!(surface, RightPanelSurface::Terminal(_)))
    }) {
        return RightPanelTerminalToggle::Show(index);
    }
    RightPanelTerminalToggle::OpenNew
}

fn reusable_surface_index(
    surfaces: &[RightPanelSurface],
    requested: &RightPanelSurface,
) -> Option<usize> {
    match requested {
        RightPanelSurface::Browser(_) | RightPanelSurface::Terminal(_) => None,
        RightPanelSurface::BackgroundWork { key, .. } => surfaces.iter().position(|surface| {
            matches!(surface, RightPanelSurface::BackgroundWork { key: candidate, .. } if candidate == key)
        }),
        RightPanelSurface::Files
        | RightPanelSurface::Diff
        | RightPanelSurface::File(_)
        | RightPanelSurface::GitGraph => {
            surfaces.iter().position(|surface| surface == requested)
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum TabScrollFadeSide {
    Left,
    Right,
}

/// Which panel a tab scroll helper addresses; the two panels keep separate
/// pending-reveal state.
#[derive(Clone, Copy)]
pub(super) enum PanelTabs {
    RightPanel,
    BottomPanel,
}

fn tab_scroll_fade_visibility(offset_x: Pixels, max_offset: Pixels) -> (bool, bool) {
    let scrolled = -offset_x;
    let threshold = px(0.5);
    (scrolled > threshold, max_offset - scrolled > threshold)
}

fn fade_safe_tab_offset(
    current_offset: Pixels,
    max_offset: Pixels,
    item_left: Pixels,
    item_right: Pixels,
    viewport_left: Pixels,
    viewport_right: Pixels,
) -> Pixels {
    let inset = px(TAB_SCROLL_FADE_WIDTH);
    let mut offset = current_offset;
    let visible_left = item_left + offset;
    let visible_right = item_right + offset;
    if visible_left < viewport_left + inset {
        offset += viewport_left + inset - visible_left;
    } else if visible_right > viewport_right - inset {
        offset -= visible_right - (viewport_right - inset);
    }
    offset.clamp(-max_offset, px(0.0))
}

pub(super) fn tab_scroll_reveal_guard(
    scroll_handle: ScrollHandle,
    tab_index: usize,
    waku: WeakEntity<Waku>,
    panel: PanelTabs,
) -> impl IntoElement {
    canvas(
        move |_, window, _| {
            if let Some(item) = scroll_handle.bounds_for_item(tab_index) {
                let viewport = scroll_handle.bounds();
                let offset = scroll_handle.offset();
                let safe_offset = fade_safe_tab_offset(
                    offset.x,
                    scroll_handle.max_offset().x,
                    item.left(),
                    item.right(),
                    viewport.left(),
                    viewport.right(),
                );
                if safe_offset != offset.x {
                    scroll_handle.set_offset(point(safe_offset, offset.y));
                }
            }

            window.on_next_frame(move |_, cx| {
                let _ = waku.update(cx, |this, cx| {
                    let pending = match panel {
                        PanelTabs::RightPanel => &mut this.right_panel_pending_tab_reveal,
                        PanelTabs::BottomPanel => &mut this.bottom_panel_pending_tab_reveal,
                    };
                    if *pending == Some(tab_index) {
                        *pending = None;
                        cx.notify();
                    }
                });
            });
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

pub(super) fn tab_scroll_fade(
    scroll_handle: ScrollHandle,
    side: TabScrollFadeSide,
    surface: Hsla,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            let (show_left, show_right) =
                tab_scroll_fade_visibility(scroll_handle.offset().x, scroll_handle.max_offset().x);
            let visible = match side {
                TabScrollFadeSide::Left => show_left,
                TabScrollFadeSide::Right => show_right,
            };
            visible.then(|| {
                let transparent = surface.opacity(0.0);
                let background = match side {
                    TabScrollFadeSide::Left => linear_gradient(
                        90.0,
                        linear_color_stop(surface, 0.0),
                        linear_color_stop(transparent, 1.0),
                    ),
                    TabScrollFadeSide::Right => linear_gradient(
                        90.0,
                        linear_color_stop(transparent, 0.0),
                        linear_color_stop(surface, 1.0),
                    ),
                };
                fill(bounds, background)
            })
        },
        |_, fade, window, _| {
            if let Some(fade) = fade {
                window.paint_quad(fade);
            }
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .when(matches!(side, TabScrollFadeSide::Left), |element| {
        element.left_0()
    })
    .when(matches!(side, TabScrollFadeSide::Right), |element| {
        element.right_0()
    })
    .w(px(TAB_SCROLL_FADE_WIDTH))
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_file_links_route_by_the_active_workspace() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let project_file = workspace.join("src/app/right_panel.rs");
        let project_file_with_line = format!("{}:1596", project_file.display());
        let project_file_with_column = format!("{}:1596:8", project_file.display());
        let relative_project_file = Path::new("src")
            .join("app")
            .join("right_panel.rs")
            .to_string_lossy()
            .into_owned();

        assert_eq!(
            transcript_link_route(&project_file_with_line, Some(workspace)),
            TranscriptLinkRoute::ProjectFile(relative_project_file.clone())
        );
        assert_eq!(
            transcript_link_route(&project_file_with_column, Some(workspace)),
            TranscriptLinkRoute::ProjectFile(relative_project_file)
        );

        let encoded_file_url =
            url::Url::from_file_path(workspace.join("My File.rs")).expect("absolute file path");
        assert_eq!(
            transcript_link_route(&format!("{encoded_file_url}#L12C4"), Some(workspace)),
            TranscriptLinkRoute::ProjectFile("My File.rs".into())
        );

        let outside_file = workspace.join("../kero/src/app.rs");
        let outside_file_with_line = format!("{}:20", outside_file.display());
        assert_eq!(
            transcript_link_route(&outside_file_with_line, Some(workspace)),
            TranscriptLinkRoute::Finder(normalized_path(&outside_file))
        );
        assert_eq!(
            transcript_link_route("https://example.com/file.rs:12", Some(workspace)),
            TranscriptLinkRoute::External
        );
    }

    #[test]
    fn relative_file_references_open_in_the_project_even_before_files_exists() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let scheme = Path::new("docs")
            .join("可编程刺激发送器方案.md")
            .to_string_lossy()
            .into_owned();
        let source = Path::new("src")
            .join("app.rs")
            .to_string_lossy()
            .into_owned();

        assert_eq!(
            transcript_link_route("docs/可编程刺激发送器方案.md", Some(workspace)),
            TranscriptLinkRoute::ProjectFile(scheme.clone())
        );
        assert_eq!(
            transcript_link_route("`docs/可编程刺激发送器方案.md`", Some(workspace)),
            TranscriptLinkRoute::ProjectFile(scheme)
        );
        assert_eq!(
            transcript_link_route("src/app.rs:12:8", Some(workspace)),
            TranscriptLinkRoute::ProjectFile(source.clone())
        );
        assert_eq!(
            transcript_link_route("src/app.rs#L12C4", Some(workspace)),
            TranscriptLinkRoute::ProjectFile(source)
        );
        assert_eq!(
            transcript_link_route("0x47", Some(workspace)),
            TranscriptLinkRoute::External
        );
        assert_eq!(
            transcript_link_route("docs/可编程刺激发送器方案.md", None),
            TranscriptLinkRoute::NoWorkspace
        );

        let outside = normalized_path(&workspace.join("../kero/src/app.rs"));
        assert_eq!(
            transcript_link_route("../kero/src/app.rs", Some(workspace)),
            TranscriptLinkRoute::Finder(outside)
        );

        let ancestors = ancestor_directories(workspace, "docs/可编程刺激发送器方案.md");
        assert_eq!(ancestors, vec![workspace.join("docs")]);
        assert!(ancestor_directories(workspace, "README.md").is_empty());
    }

    #[test]
    fn home_references_expand_to_the_user_home_not_the_workspace() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let home = dirs::home_dir().expect("a home directory");
        let dotfile = normalized_path(&home.join(".zshrc"));

        for reference in ["~/.zshrc", "~/.zshrc:12", "`~/.zshrc`"] {
            assert_eq!(
                transcript_link_route(reference, Some(workspace)),
                TranscriptLinkRoute::Finder(dotfile.clone())
            );
        }
    }

    #[test]
    fn remote_posix_workspaces_route_with_forward_slashes_on_every_desktop() {
        let workspace = Path::new("/home/me/proj");

        assert_eq!(
            transcript_link_route("docs/guide.md", Some(workspace)),
            TranscriptLinkRoute::ProjectFile("docs/guide.md".into())
        );
        assert_eq!(
            transcript_link_route("/home/me/proj/docs/guide.md:12", Some(workspace)),
            TranscriptLinkRoute::ProjectFile("docs/guide.md".into())
        );
        assert_eq!(
            transcript_link_route("/home/me/proj/docs/My%20Guide.md", Some(workspace)),
            TranscriptLinkRoute::ProjectFile("docs/My Guide.md".into())
        );
        assert_eq!(
            transcript_link_route("file:///home/me/proj/docs/guide.md#L3", Some(workspace)),
            TranscriptLinkRoute::ProjectFile("docs/guide.md".into())
        );
        assert_eq!(
            transcript_link_route("../other/notes.md", Some(workspace)),
            TranscriptLinkRoute::Finder(PathBuf::from("/home/me/other/notes.md"))
        );
        assert_eq!(
            transcript_link_route("/home/me/proj", Some(workspace)),
            TranscriptLinkRoute::Finder(PathBuf::from("/home/me/proj"))
        );
        assert_eq!(
            transcript_link_route("https://example.com/guide.md", Some(workspace)),
            TranscriptLinkRoute::External
        );
        // `~/x` names the daemon user's home; it must never be joined onto
        // the workspace as a relative path.
        #[cfg(windows)]
        assert_eq!(
            transcript_link_route("~/.zshrc", Some(workspace)),
            TranscriptLinkRoute::Finder(PathBuf::from("~/.zshrc"))
        );
        #[cfg(not(windows))]
        assert_eq!(
            transcript_link_route("~/.zshrc", Some(workspace)),
            TranscriptLinkRoute::Finder(dirs::home_dir().unwrap().join(".zshrc"))
        );

        // Expanded rows are keyed by the daemon's own absolute paths.
        assert_eq!(
            ancestor_directories(Path::new("/home/me/proj/"), "docs/api/guide.md"),
            vec![
                PathBuf::from("/home/me/proj/docs"),
                PathBuf::from("/home/me/proj/docs/api"),
            ]
        );
        assert!(ancestor_directories(workspace, "README.md").is_empty());
    }

    /// Selection resolves rows by key, so a repeated key makes a drag jump
    /// between the duplicates. Numbered rows keep their number-derived keys
    /// (stable across Review's gap expansion); rows a provider never
    /// positioned fall back to the row index.
    #[test]
    fn diff_row_selection_keys_are_unique_even_without_line_numbers() {
        let positionless =
            crate::review_diff::from_file_changes(&[crate::model::ActivityFileChange {
                path: "a.md".into(),
                additions: Some(2),
                deletions: Some(0),
                status: None,
                diff: Some("@@\n+one\n+two\n \n+three\n".into()),
            }]);
        let keys = positionless
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                matches!(
                    line.kind,
                    crate::review_diff::LineKind::Context
                        | crate::review_diff::LineKind::Addition
                        | crate::review_diff::LineKind::Deletion
                )
            })
            .map(|(index, line)| diff_row_selection_key("activity", line, index))
            .collect::<Vec<_>>();
        let unique = keys.iter().collect::<HashSet<_>>();
        assert_eq!(unique.len(), keys.len(), "{keys:?}");

        let numbered = crate::review_diff::Line {
            file_index: 0,
            old_line: Some(4),
            new_line: Some(6),
            kind: crate::review_diff::LineKind::Context,
            content: "kept".into(),
            tokens: Vec::new(),
        };
        assert_eq!(
            diff_row_selection_key("review-diff", &numbered, 9),
            "review-diff-line-0-context-4-6",
        );
    }

    fn review_file(path: &str) -> crate::review_diff::File {
        crate::review_diff::File {
            path: path.into(),
            additions: 1,
            deletions: 0,
            status: crate::review_diff::FileStatus::Modified,
            diff_line: None,
        }
    }

    fn review_files() -> Vec<crate::review_diff::File> {
        [
            "README.md",
            "src/app/runtime.rs",
            "src/app/view.rs",
            "src/lib.rs",
            "tests/review.rs",
        ]
        .into_iter()
        .map(review_file)
        .collect()
    }

    #[test]
    fn review_gap_expansion_icons_match_pierre_visual_directions() {
        use crate::review_diff::{ExpansionDirection, GapPosition};

        assert_eq!(
            review_diff_gap_directions(GapPosition::Leading, true),
            &[ExpansionDirection::End]
        );
        assert_eq!(
            review_diff_gap_directions(GapPosition::Trailing, true),
            &[ExpansionDirection::Start]
        );
        assert_eq!(
            review_diff_gap_directions(GapPosition::Between, false),
            &[ExpansionDirection::Both]
        );
        assert_eq!(
            review_diff_gap_directions(GapPosition::Between, true),
            &[ExpansionDirection::Start, ExpansionDirection::End]
        );

        assert_eq!(
            review_diff_gap_icon_path(ExpansionDirection::Start),
            "icons/chevron-down.svg"
        );
        assert_eq!(
            review_diff_gap_icon_path(ExpansionDirection::End),
            "icons/chevron-up.svg"
        );
        assert_eq!(
            review_diff_gap_icon_path(ExpansionDirection::Both),
            "icons/chevrons-up-down.svg"
        );
    }

    #[test]
    fn review_tree_builds_shared_directories_once() {
        let files = review_files();
        let expanded = review_diff_directory_paths(&files);
        assert_eq!(
            review_diff_tree_rows(&files, &expanded, ""),
            vec![
                ReviewDiffTreeRow::File {
                    file_index: 0,
                    depth: 0,
                },
                ReviewDiffTreeRow::Directory {
                    path: "src".into(),
                    name: "src".into(),
                    depth: 0,
                    expanded: true,
                },
                ReviewDiffTreeRow::Directory {
                    path: "src/app".into(),
                    name: "app".into(),
                    depth: 1,
                    expanded: true,
                },
                ReviewDiffTreeRow::File {
                    file_index: 1,
                    depth: 2,
                },
                ReviewDiffTreeRow::File {
                    file_index: 2,
                    depth: 2,
                },
                ReviewDiffTreeRow::File {
                    file_index: 3,
                    depth: 1,
                },
                ReviewDiffTreeRow::Directory {
                    path: "tests".into(),
                    name: "tests".into(),
                    depth: 0,
                    expanded: true,
                },
                ReviewDiffTreeRow::File {
                    file_index: 4,
                    depth: 1,
                },
            ]
        );
    }

    #[test]
    fn review_tree_collapse_hides_only_descendants() {
        let files = review_files();
        let expanded = HashSet::from(["src".to_owned()]);
        let rows = review_diff_tree_rows(&files, &expanded, "");

        assert!(rows.contains(&ReviewDiffTreeRow::Directory {
            path: "src/app".into(),
            name: "app".into(),
            depth: 1,
            expanded: false,
        }));
        assert!(rows.contains(&ReviewDiffTreeRow::File {
            file_index: 3,
            depth: 1,
        }));
        assert!(!rows.iter().any(|row| {
            matches!(
                row,
                ReviewDiffTreeRow::File { file_index, .. } if *file_index == 1 || *file_index == 2
            )
        }));
    }

    #[test]
    fn review_tree_filter_reveals_matching_path_and_ancestors() {
        let rows = review_diff_tree_rows(&review_files(), &HashSet::new(), "RUNTIME");
        assert_eq!(
            rows,
            vec![
                ReviewDiffTreeRow::Directory {
                    path: "src".into(),
                    name: "src".into(),
                    depth: 0,
                    expanded: true,
                },
                ReviewDiffTreeRow::Directory {
                    path: "src/app".into(),
                    name: "app".into(),
                    depth: 1,
                    expanded: true,
                },
                ReviewDiffTreeRow::File {
                    file_index: 1,
                    depth: 2,
                },
            ]
        );
    }

    #[test]
    fn review_render_path_only_reads_the_in_memory_snapshot() {
        let source = include_str!("right_panel.rs");
        let start = source
            .find("\n    fn render_right_panel_diff(")
            .expect("review render fn");
        let body = &source[start + 1..];
        let end = body
            .find("\n    pub(super) fn render_right_panel_empty_message(")
            .expect("review render end");
        let body = &body[..end];

        for forbidden in [
            "Command::new",
            "std::fs::",
            "review_diff::collect",
            "capture_worktree_commit",
        ] {
            assert!(
                !body.contains(forbidden),
                "Review rendering must not call `{forbidden}`; prepare it in refresh_right_panel_diff"
            );
        }
    }

    /// A wrapped diff line must grow its row rather than be clipped by it.
    /// Both the panel's own rows and the shared code row have to hold this,
    /// and the shared one is also what the transcript's diff paints with.
    #[test]
    fn diff_text_rows_soft_wrap() {
        let source = include_str!("right_panel.rs");
        let panel = source
            .split_once("\n    fn render_right_panel_diff_line(")
            .expect("review diff line renderer")
            .1
            .split_once("\n    #[allow(clippy::too_many_arguments)]")
            .expect("review diff line renderer end")
            .0;
        let shared = source
            .split_once("\npub(super) fn render_diff_code_row(")
            .expect("shared diff code row")
            .1
            .split_once("\nfn review_diff_flat_text(")
            .expect("shared diff code row end")
            .0;

        for body in [panel, shared] {
            assert!(!body.contains(".whitespace_nowrap()"));
        }
        assert!(panel.matches(".whitespace_normal()").count() >= 2);
        assert!(shared.contains(".whitespace_normal()"));
        assert!(shared.contains(".min_h(px(style.row_height))"));
        assert!(!shared.contains(".h(px(style.row_height))"));
    }

    /// The render path must never reach the filesystem. This reads the source
    /// rather than the behaviour, because the cost of a regression here is a
    /// syscall per directory entry on every frame — invisible until a project
    /// is large or its volume is slow.
    #[test]
    fn the_working_tree_render_path_does_no_filesystem_work() {
        let source = include_str!("right_panel.rs");
        // Anchored on the definition's indentation so this test does not match
        // its own string literals.
        let start = source
            .find("\n    fn render_right_panel_working_tree(")
            .expect("render fn");
        let body = &source[start + 1..];
        let end = body.find("\n    fn ").unwrap_or(body.len());
        let body = &body[..end];

        for forbidden in [
            "visible_working_tree_entries",
            "read_dir",
            "std::fs::",
            "metadata(",
        ] {
            assert!(
                !body.contains(forbidden),
                "render_right_panel_working_tree must not call `{forbidden}`; \
                 walk the tree in refresh_right_panel_working_tree instead"
            );
        }
    }

    /// Same guard for the file editor, which `render_right_panel_file` reaches
    /// on every frame that draws a file tab. Opening a large file used to read
    /// it inline, so the frame that revealed the tab paid for the whole file.
    #[test]
    fn the_file_editor_render_path_does_no_filesystem_work() {
        let source = include_str!("right_panel.rs");
        let start = source
            .find("\n    fn ensure_right_panel_file_editor(")
            .expect("ensure fn");
        let body = &source[start + 1..];
        let end = body
            .find("\n    /// Reads a file into its editor")
            .unwrap_or(body.len());
        let body = &body[..end];

        for forbidden in ["read_right_panel_file(", "std::fs::", "metadata("] {
            assert!(
                !body.contains(forbidden),
                "ensure_right_panel_file_editor must not call `{forbidden}`; \
                 read the file in read_right_panel_file_into_editor instead"
            );
        }
    }

    #[test]
    fn working_tree_only_descends_into_expanded_directories() {
        let root = std::env::temp_dir().join(format!("waku-working-tree-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "# Waku\n").unwrap();

        let collapsed = visible_working_tree_entries(&root, &HashSet::new());
        assert_eq!(
            collapsed
                .iter()
                .map(|entry| entry.relative_path.clone())
                .collect::<Vec<_>>(),
            vec!["src".to_owned(), "README.md".to_owned()]
        );

        let expanded = HashSet::from([root.join("src")]);
        let visible = visible_working_tree_entries(&root, &expanded);
        let nested = Path::new("src")
            .join("nested")
            .to_string_lossy()
            .into_owned();
        let main_rs = Path::new("src")
            .join("main.rs")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            visible
                .iter()
                .map(|entry| (entry.relative_path.clone(), entry.depth))
                .collect::<Vec<_>>(),
            vec![
                ("src".to_owned(), 0),
                (nested, 1),
                (main_rs, 1),
                ("README.md".to_owned(), 0)
            ]
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_highlighter_language_follows_file_name_and_extension() {
        assert_eq!(file_highlighter_language("src/app.rs"), "rust");
        assert_eq!(file_highlighter_language("ui/panel.tsx"), "tsx");
        assert_eq!(file_highlighter_language("Sources/App.swift"), "swift");
        assert_eq!(file_highlighter_language("Makefile"), "make");
        assert_eq!(file_highlighter_language("src/native.hpp"), "cpp");
        assert_eq!(file_highlighter_language("LICENSE"), "text");

        for (path, expected_language) in [
            ("bun.lock", "json"),
            ("package-lock.json", "json"),
            ("deno.lock", "json"),
            ("composer.lock", "json"),
            ("Pipfile.lock", "json"),
            ("Package.resolved", "json"),
            ("Cargo.lock", "toml"),
            ("uv.lock", "toml"),
            ("poetry.lock", "toml"),
            ("pnpm-lock.yaml", "yaml"),
            ("yarn.lock", "yaml"),
            ("Podfile.lock", "yaml"),
            ("Gemfile.lock", "yaml"),
            ("mix.lock", "elixir"),
        ] {
            assert_eq!(file_highlighter_language(path), expected_language, "{path}");
        }
    }

    /// The editor colours code with the in-house lexer, so what matters is that
    /// the names `file_highlighter_language` produces are ones the lexer knows.
    /// The few it does not are listed here deliberately: they render as plain
    /// monospace rather than silently looking broken.
    #[test]
    fn mapped_languages_resolve_in_the_in_house_lexer() {
        use crate::md::highlight::{Lang, lang_for_tag};

        for (language, expected) in [
            ("rust", Some(Lang::Rust)),
            ("tsx", Some(Lang::Script)),
            ("swift", Some(Lang::Swift)),
            ("json", Some(Lang::Json)),
            ("toml", Some(Lang::Toml)),
            ("yaml", Some(Lang::Yaml)),
            ("make", Some(Lang::Shell)),
            ("cpp", Some(Lang::C)),
            ("markdown", Some(Lang::Markdown)),
            // Not yet lexed; these fall back to unhighlighted monospace.
            ("elixir", None),
            ("text", None),
        ] {
            assert_eq!(lang_for_tag(language), expected, "{language}");
        }
    }

    #[test]
    fn the_editor_lexer_colours_code_it_recognises() {
        use crate::md::highlight::{Carry, Lang, TokenClass, tokenize_line};

        let line = r#"export function Card({ title }: { title: string }) {"#;
        let spans = tokenize_line(Lang::Script, line, Carry::None)
            .0
            .into_iter()
            .map(|token| (&line[token.range], token.class))
            .collect::<Vec<_>>();

        assert!(spans.contains(&("export", TokenClass::Keyword)));
        assert!(spans.contains(&("function", TokenClass::Keyword)));
        assert!(spans.contains(&("Card", TokenClass::Function)));
    }

    #[test]
    fn working_tree_file_icons_follow_names_and_extensions() {
        assert_eq!(file_icon_for_name("main.rs"), "icons/file-types/rust.svg");
        assert_eq!(
            file_icon_for_name("Panel.tsx"),
            "icons/file-types/react.svg"
        );
        assert_eq!(
            file_icon_for_name("README.md"),
            "icons/file-types/readme.svg"
        );
        assert_eq!(
            file_icon_for_name("Dockerfile.dev"),
            "icons/file-types/docker.svg"
        );
        assert_eq!(file_icon_for_name("bun.lock"), "icons/file-types/bun.svg");
        assert_eq!(
            file_icon_for_name("pnpm-lock.yaml"),
            "icons/file-types/pnpm.svg"
        );
        assert_eq!(
            file_icon_for_name("vite.config.ts"),
            "icons/file-types/vite.svg"
        );
        assert_eq!(
            file_icon_for_name("unknown.data"),
            "icons/file-types/file.svg"
        );
    }

    #[test]
    fn files_tab_uses_the_selected_file_name_and_icon() {
        let files = RightPanelSurface::Files;
        assert_eq!(right_panel_tab_label(&files, None), "Files");
        assert_eq!(
            right_panel_tab_label(&files, Some("packages/desktop/bun.lock")),
            "bun.lock"
        );
        assert_eq!(
            right_panel_tab_icon(&files, Some("packages/desktop/bun.lock")),
            "icons/file-types/bun.svg"
        );

        let file = RightPanelSurface::File("src/main.rs".into());
        assert_eq!(right_panel_tab_label(&file, None), "main.rs");
        assert_eq!(
            right_panel_tab_icon(&file, None),
            "icons/file-types/rust.svg"
        );
    }

    #[test]
    fn right_panel_tab_titles_stay_on_one_line() {
        let source = include_str!("right_panel.rs");
        let header = source
            .split_once("\n    fn render_right_panel_header(")
            .expect("right panel header renderer")
            .1
            .split_once("\n    fn render_right_panel_chooser(")
            .expect("right panel header renderer end")
            .0;

        assert!(header.contains(".truncate()"));
        assert!(!header.contains(".line_clamp(1)"));

        let background = RightPanelSurface::BackgroundWork {
            key: BackgroundWorkKey::new(BackgroundWorkKind::Process, "process-1"),
            title: "node -e '\n  const value = 1'".into(),
        };
        assert_eq!(
            right_panel_tab_label(&background, None),
            "node -e ' const value = 1'"
        );
    }

    #[test]
    fn only_reuses_single_instance_surface_tabs() {
        let browser = RightPanelSurface::new_browser();
        let terminal = RightPanelSurface::new_terminal();
        let background = RightPanelSurface::BackgroundWork {
            key: BackgroundWorkKey::new(BackgroundWorkKind::Process, "process-1"),
            title: "Process one".into(),
        };
        let surfaces = vec![
            browser,
            terminal,
            background,
            RightPanelSurface::Files,
            RightPanelSurface::Diff,
            RightPanelSurface::GitGraph,
        ];

        assert_eq!(
            reusable_surface_index(&surfaces, &RightPanelSurface::new_browser()),
            None
        );
        assert_eq!(
            reusable_surface_index(&surfaces, &RightPanelSurface::new_terminal()),
            None
        );
        assert_eq!(
            reusable_surface_index(
                &surfaces,
                &RightPanelSurface::BackgroundWork {
                    key: BackgroundWorkKey::new(BackgroundWorkKind::Process, "process-1"),
                    title: "Renamed process".into(),
                },
            ),
            Some(2)
        );
        assert_eq!(
            reusable_surface_index(&surfaces, &RightPanelSurface::Files),
            Some(3)
        );
        assert_eq!(
            reusable_surface_index(&surfaces, &RightPanelSurface::Diff),
            Some(4)
        );
        assert_eq!(
            reusable_surface_index(&surfaces, &RightPanelSurface::GitGraph),
            Some(5)
        );
    }

    #[test]
    fn header_terminal_button_reuses_a_terminal_tab() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let surfaces = vec![
            RightPanelSurface::Files,
            RightPanelSurface::Terminal(first),
            RightPanelSurface::GitGraph,
            RightPanelSurface::Terminal(second),
        ];

        assert_eq!(
            right_panel_terminal_toggle(true, &surfaces, Some(1)),
            RightPanelTerminalToggle::Hide
        );
        // Hidden while a terminal is active: show that same tab, not a new one.
        assert_eq!(
            right_panel_terminal_toggle(false, &surfaces, Some(3)),
            RightPanelTerminalToggle::Show(3)
        );
        // Another surface is showing: jump to the first terminal tab.
        assert_eq!(
            right_panel_terminal_toggle(true, &surfaces, Some(0)),
            RightPanelTerminalToggle::Show(1)
        );
        assert_eq!(
            right_panel_terminal_toggle(false, &[RightPanelSurface::Files], Some(0)),
            RightPanelTerminalToggle::OpenNew
        );
    }

    #[test]
    fn right_panel_state_isolated_by_workspace() {
        let scope = PanelScope {
            host: HostKey::Local,
            workspace: PathBuf::from("/repos/waku"),
        };
        let other_scope = PanelScope {
            host: HostKey::Local,
            workspace: PathBuf::from("/repos/other"),
        };
        let terminal_id = Uuid::new_v4();
        let mut states = HashMap::new();
        let mut terminal_state = RightPanelWorkspaceState::empty(true);
        terminal_state.surfaces = vec![RightPanelSurface::Terminal(terminal_id)];
        terminal_state.active_surface = Some(0);
        terminal_state.file_tree_width = 248.0;
        states.insert(scope.clone(), terminal_state);

        // A workspace with no stored panel state opens closed.
        let other_state = RightPanelWorkspaceState::take_or_closed(&mut states, &other_scope);
        assert!(!other_state.visible);
        assert!(other_state.surfaces.is_empty());
        assert_eq!(other_state.active_surface, None);
        assert_eq!(other_state.file_tree_width, DEFAULT_FILE_TREE_WIDTH);

        // Sessions of the same workspace all find the same state, which is
        // the point: creating or switching sessions keeps the panels.
        let restored = RightPanelWorkspaceState::take_or_closed(&mut states, &scope);
        assert!(restored.visible);
        assert_eq!(
            restored.surfaces,
            vec![RightPanelSurface::Terminal(terminal_id)]
        );
        assert_eq!(restored.active_surface, Some(0));
        assert_eq!(restored.file_tree_width, 248.0);
    }

    #[test]
    fn closed_session_surfaces_drop_only_session_owned_tabs() {
        let session = Uuid::new_v4();
        let other_session = Uuid::new_v4();
        let work_key = BackgroundWorkKey::new(BackgroundWorkKind::Process, "process-1");
        let terminal_id = Uuid::new_v4();
        let mut state = RightPanelWorkspaceState::empty(true);
        state.surfaces = vec![
            RightPanelSurface::Files,
            RightPanelSurface::Diff,
            RightPanelSurface::BackgroundWork {
                key: work_key.clone(),
                title: "Process one".into(),
            },
            RightPanelSurface::Terminal(terminal_id),
        ];
        state.active_surface = Some(1);
        state.diff_source = ReviewDiffSource::LastTurn {
            session_id: session,
            turn_id: Uuid::new_v4(),
            turn_count: 3,
        };
        let work_keys = HashSet::from([work_key]);

        state.close_session_surfaces(session, &work_keys);
        // The workspace's Files tab and terminal stay; the session's diff and
        // background work close with it.
        assert_eq!(
            state.surfaces,
            vec![
                RightPanelSurface::Files,
                RightPanelSurface::Terminal(terminal_id)
            ]
        );
        assert!(state.visible);
        // The active Diff tab closed; the panel steps to a remaining tab.
        assert_eq!(state.active_surface, Some(0));

        // A diff owned by another session — or a workspace review — stays.
        state.surfaces = vec![
            RightPanelSurface::Files,
            RightPanelSurface::Diff,
            RightPanelSurface::GitGraph,
        ];
        state.active_surface = Some(1);
        state.diff_source = ReviewDiffSource::LastTurn {
            session_id: other_session,
            turn_id: Uuid::new_v4(),
            turn_count: 1,
        };
        state.close_session_surfaces(session, &HashSet::new());
        assert_eq!(state.surfaces.len(), 3);
        assert_eq!(state.active_surface, Some(1));

        // Stripping the last tab closes the panel instead of orphaning it.
        state.surfaces = vec![RightPanelSurface::Diff];
        state.active_surface = Some(0);
        state.diff_source = ReviewDiffSource::LastTurn {
            session_id: session,
            turn_id: Uuid::new_v4(),
            turn_count: 2,
        };
        state.close_session_surfaces(session, &HashSet::new());
        assert!(state.surfaces.is_empty());
        assert!(!state.visible);
        assert_eq!(state.active_surface, None);
        assert_eq!(state.pending_tab_reveal, None);
    }

    #[test]
    fn tab_scroll_fades_only_show_toward_hidden_content() {
        assert_eq!(
            tab_scroll_fade_visibility(px(0.0), px(120.0)),
            (false, true)
        );
        assert_eq!(
            tab_scroll_fade_visibility(px(-40.0), px(120.0)),
            (true, true)
        );
        assert_eq!(
            tab_scroll_fade_visibility(px(-120.0), px(120.0)),
            (true, false)
        );
        assert_eq!(tab_scroll_fade_visibility(px(0.0), px(0.0)), (false, false));
    }

    #[test]
    fn selected_tab_offset_clears_fade_overlays() {
        assert_eq!(
            fade_safe_tab_offset(
                px(-100.0),
                px(300.0),
                px(90.0),
                px(190.0),
                px(0.0),
                px(300.0),
            ),
            px(-66.0)
        );
        assert_eq!(
            fade_safe_tab_offset(
                px(-100.0),
                px(324.0),
                px(300.0),
                px(400.0),
                px(0.0),
                px(300.0),
            ),
            px(-124.0)
        );
        assert_eq!(
            fade_safe_tab_offset(px(0.0), px(0.0), px(0.0), px(100.0), px(0.0), px(300.0),),
            px(0.0)
        );
    }
}

impl Waku {
    pub(super) fn open_transcript_link(&mut self, target: &str, cx: &mut Context<Self>) -> bool {
        let location =
            crate::md::parser::parse_file_location(target.trim().trim_matches(['"', '\'', '`'])).1;
        match transcript_link_route(target, self.selected_workspace_path()) {
            TranscriptLinkRoute::ProjectFile(relative_path) => {
                // Files may never have been opened. Create that surface, expand
                // every parent so the row exists in the first tree walk, then
                // select the file inside it.
                if let Some(workspace) = self.selected_workspace_path().map(Path::to_path_buf) {
                    for directory in ancestor_directories(&workspace, &relative_path) {
                        self.right_panel_expanded_paths.insert(directory);
                    }
                }
                self.open_right_panel_surface(RightPanelSurface::Files, cx);
                self.open_right_panel_file_at(relative_path, location, cx);
            }
            TranscriptLinkRoute::Finder(path) => {
                if self.selected_daemon().is_remote() {
                    self.show_toast(tr!("errors.remote_host_path"));
                    cx.notify();
                } else {
                    crate::platform::reveal_in_file_manager(&path, cx);
                }
            }
            TranscriptLinkRoute::NoWorkspace => {
                self.show_toast(tr!("files.no_project_open"));
                cx.notify();
            }
            TranscriptLinkRoute::External => return false,
        }
        true
    }

    /// Open a path a tool reported, from an activity in the transcript.
    ///
    /// Providers name a changed file however they like — absolute, or relative
    /// to the session's workspace — so resolve it before routing. Inside the
    /// workspace it opens in the file viewer; anywhere else it goes to the file
    /// manager, the same split a file link in the transcript takes.
    pub(super) fn open_activity_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if let Some(workspace) = self.selected_workspace_path().and_then(posix_workspace) {
            let path = path.trim();
            let resolved = if path.starts_with('/') {
                path.to_owned()
            } else {
                format!("{}/{path}", workspace.trim_end_matches('/'))
            };
            self.open_transcript_link(&resolved, cx);
            return;
        }
        let path = Path::new(path.trim());
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else if let Some(workspace) = self.selected_workspace_path() {
            workspace.join(path)
        } else {
            return;
        };
        self.open_transcript_link(&resolved.to_string_lossy(), cx);
    }

    /// Route a file path clicked in a terminal: files inside the workspace
    /// open in the Files panel; directories or external paths open in the
    /// system file manager.
    pub(super) fn open_terminal_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !path.is_dir() && self.selected_workspace_path().is_some() {
            self.open_activity_file(&path.to_string_lossy(), cx);
        } else {
            crate::platform::reveal_in_file_manager(path, cx);
        }
    }

    /// Park the selected workspace's panel state under its scope. Terminal
    /// and browser entities move into the map, so their processes keep
    /// running off screen while another workspace is shown.
    pub(super) fn store_selected_right_panel_state(&mut self) {
        let Some(scope) = self
            .state
            .selected_session
            .and_then(|id| self.panel_scope_for_session(id))
        else {
            return;
        };
        let state = self.take_active_right_panel_state();
        self.right_panel_workspace_states.insert(scope, state);
    }

    pub(super) fn restore_right_panel_state(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let state = self
            .panel_scope_for_session(session_id)
            .map(|scope| {
                RightPanelWorkspaceState::take_or_closed(
                    &mut self.right_panel_workspace_states,
                    &scope,
                )
            })
            .unwrap_or_else(|| RightPanelWorkspaceState::empty(false));
        self.replace_active_right_panel_state(state);
        self.sync_right_panel_diff_tree_rows(cx);
        // A read in flight when this session was switched away from had its
        // result dropped, and the flag it left behind would stop the editor
        // ever asking again. Clear it and read afresh, which also picks up
        // edits made while another session was on screen.
        for editor in self.right_panel_file_editors.values_mut() {
            editor.reading = false;
        }
        // The find bar pointed into the editors that were just swapped out;
        // its match list means nothing here, and restored editors may carry
        // washes stored mid-search.
        self.reset_file_search_for_session(cx);
        self.reload_clean_right_panel_file_editors(cx);
        self.state.right_panel_visible = self.right_panel_visible;
        if self.active_right_panel_surface() == Some(&RightPanelSurface::Diff) {
            self.refresh_right_panel_diff(cx);
        }
        if matches!(
            self.active_right_panel_surface(),
            Some(RightPanelSurface::Files | RightPanelSurface::File(_))
        ) {
            self.refresh_right_panel_working_tree(cx);
        }
        self.ensure_right_panel_terminals(cx);
        self.retain_right_panel_browsers();
        if self.right_panel_visible {
            self.request_active_terminal_focus();
            self.request_active_browser_focus();
        }
    }

    /// Remove a deleted session's share of the panel state. A review pinned
    /// to one of its turns and the background work it reported close with
    /// it; everything else belongs to the workspace and survives while any
    /// other session still opens into it. Only when the last session of a
    /// workspace goes away do the terminals and browsers shut down.
    pub(super) fn remove_right_panel_session_state(&mut self, session_id: Uuid) {
        let Some(scope) = self.panel_scope_for_session(session_id) else {
            return;
        };
        let work_keys = self.background_work_keys(session_id);
        let workspace_still_open = self.state.sessions.iter().any(|session| {
            session.id != session_id
                && self.panel_scope_for_session(session.id).as_ref() == Some(&scope)
        });
        let state = if self.state.selected_session == Some(session_id) {
            let state = self.take_active_right_panel_state();
            self.replace_active_right_panel_state(RightPanelWorkspaceState::empty(false));
            Some(state)
        } else {
            self.right_panel_workspace_states.remove(&scope)
        };
        let Some(mut state) = state else {
            // Nothing parked under the scope: its state is live on screen
            // for another session of the same workspace, so the map held
            // nothing to take. The deleted session's share of the live
            // state — a review pinned to one of its turns, background work
            // it reported — would otherwise survive there as an orphan tab.
            let live_scope = self
                .state
                .selected_session
                .and_then(|id| self.panel_scope_for_session(id));
            if workspace_still_open && live_scope.as_ref() == Some(&scope) {
                self.close_live_session_surfaces(session_id, &work_keys);
            }
            return;
        };
        if workspace_still_open {
            state.close_session_surfaces(session_id, &work_keys);
            self.right_panel_workspace_states.insert(scope, state);
        } else {
            for surface in &state.surfaces {
                if let Some(terminal_id) = surface.terminal_id() {
                    self.right_panel_terminals.remove(&terminal_id);
                }
                if let Some(browser_id) = surface.browser_id() {
                    self.right_panel_browsers.remove(&browser_id);
                }
            }
        }
    }

    /// `close_session_surfaces` on the state that is live on screen. Only the
    /// tab strip moves through it: a full take/replace would also abandon an
    /// open rename, drop an in-flight diff load and reset the diff view of a
    /// session the user is still looking at.
    fn close_live_session_surfaces(
        &mut self,
        session_id: Uuid,
        work_keys: &HashSet<BackgroundWorkKey>,
    ) {
        let mut tabs = RightPanelWorkspaceState::empty(self.right_panel_visible);
        tabs.surfaces = std::mem::take(&mut self.right_panel_surfaces);
        tabs.active_surface = self.right_panel_active_surface;
        tabs.pending_tab_reveal = self.right_panel_pending_tab_reveal;
        tabs.diff_source = self.right_panel_diff_source;
        tabs.close_session_surfaces(session_id, work_keys);
        self.right_panel_surfaces = tabs.surfaces;
        self.right_panel_active_surface = tabs.active_surface;
        self.right_panel_pending_tab_reveal = tabs.pending_tab_reveal;
        self.right_panel_visible = tabs.visible;
        self.state.right_panel_visible = self.right_panel_visible;
    }

    fn take_active_right_panel_state(&mut self) -> RightPanelWorkspaceState {
        RightPanelWorkspaceState {
            visible: self.right_panel_visible,
            surfaces: std::mem::take(&mut self.right_panel_surfaces),
            active_surface: self.right_panel_active_surface.take(),
            tabs_scroll_handle: std::mem::replace(
                &mut self.right_panel_tabs_scroll_handle,
                ScrollHandle::new(),
            ),
            pending_tab_reveal: self.right_panel_pending_tab_reveal.take(),
            expanded_paths: std::mem::take(&mut self.right_panel_expanded_paths),
            files_selected_path: self.right_panel_files_selected_path.take(),
            file_tree_width: self.right_panel_file_tree_width,
            file_editors: std::mem::take(&mut self.right_panel_file_editors),
            diff_source: self.right_panel_diff_source,
            diff_snapshot: self.right_panel_diff_snapshot.take(),
            diff_selected_file: self.right_panel_diff_selected_file.take(),
            diff_expanded_paths: std::mem::take(&mut self.right_panel_diff_expanded_paths),
        }
    }

    fn replace_active_right_panel_state(&mut self, state: RightPanelWorkspaceState) {
        self.right_panel_visible = state.visible;
        self.right_panel_surfaces = state.surfaces;
        self.right_panel_active_surface = state.active_surface;
        self.right_panel_tabs_scroll_handle = state.tabs_scroll_handle;
        self.right_panel_pending_tab_reveal = state.pending_tab_reveal;
        self.right_panel_expanded_paths = state.expanded_paths;
        self.right_panel_files_selected_path = state.files_selected_path;
        self.right_panel_file_tree_width = state.file_tree_width;
        self.right_panel_file_editors = state.file_editors;
        // A rename or delete confirm belongs to the tree it was opened on;
        // switching sessions abandons it rather than retargeting the wrong row.
        self.right_panel_file_rename = None;
        self.right_panel_delete_confirm = None;
        self.right_panel_diff_generation = self.right_panel_diff_generation.wrapping_add(1);
        self.right_panel_diff_selection.clear();
        self.right_panel_diff_source = state.diff_source;
        self.right_panel_diff_snapshot = state.diff_snapshot;
        self.right_panel_diff_loading = false;
        self.right_panel_diff_error = None;
        self.right_panel_diff_selected_file = state.diff_selected_file;
        self.right_panel_diff_expanded_paths = state.diff_expanded_paths;
        self.right_panel_diff_tree_cursor = None;
        self.right_panel_diff_tree_rows.borrow_mut().clear();
        self.right_panel_diff_tree_list_state.reset(0);
        let line_count = self
            .right_panel_diff_snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.lines.len());
        self.right_panel_diff_list_state.reset(line_count);
    }

    fn reveal_right_panel_tab(&mut self, index: usize) {
        self.right_panel_pending_tab_reveal = Some(index);
        self.right_panel_tabs_scroll_handle.scroll_to_item(index);
    }

    fn active_right_panel_surface(&self) -> Option<&RightPanelSurface> {
        self.right_panel_active_surface
            .and_then(|index| self.right_panel_surfaces.get(index))
    }

    pub(super) fn request_active_terminal_focus(&mut self) {
        self.right_panel_pending_terminal_focus = self
            .active_right_panel_surface()
            .and_then(RightPanelSurface::terminal_id);
    }

    pub(super) fn request_active_browser_focus(&mut self) {
        self.right_panel_pending_browser_focus = self
            .active_right_panel_surface()
            .and_then(RightPanelSurface::browser_id);
    }

    /// The file the active editor surface is showing, whether via a File tab
    /// or the Files browser's selection — regardless of whether the panel is
    /// currently visible, which is a per-caller decision: save works on a
    /// hidden panel, find does not.
    pub(super) fn visible_right_panel_file_path(&self) -> Option<String> {
        match self.active_right_panel_surface() {
            Some(RightPanelSurface::Files) => self.right_panel_files_selected_path.clone(),
            Some(RightPanelSurface::File(path)) => Some(path.clone()),
            _ => None,
        }
    }

    fn right_panel_file_is_dirty(&self, relative_path: &str) -> bool {
        self.right_panel_file_editors
            .get(relative_path)
            .is_some_and(|editor| editor.dirty)
    }

    fn right_panel_surface_is_dirty(&self, surface: &RightPanelSurface) -> bool {
        match surface {
            RightPanelSurface::Files => self
                .right_panel_files_selected_path
                .as_deref()
                .is_some_and(|path| self.right_panel_file_is_dirty(path)),
            RightPanelSurface::File(path) => self.right_panel_file_is_dirty(path),
            _ => false,
        }
    }

    fn ensure_initial_right_panel_file_editor_width(&mut self) {
        if self.right_panel_file_editors.is_empty() {
            self.right_panel_width = widened_panel_width_for_file_editor(
                self.right_panel_width,
                self.right_panel_file_tree_width,
            );
        }
    }

    pub(super) fn open_right_panel_surface(
        &mut self,
        surface: RightPanelSurface,
        cx: &mut Context<Self>,
    ) {
        let reusable_index = reusable_surface_index(&self.right_panel_surfaces, &surface);
        if matches!(&surface, RightPanelSurface::File(_)) {
            self.ensure_initial_right_panel_file_editor_width();
        }
        if surface == RightPanelSurface::Diff {
            if reusable_index.is_none() {
                self.right_panel_width = widened_panel_width_for_review(self.right_panel_width);
            }
            self.refresh_right_panel_diff(cx);
        }
        if surface == RightPanelSurface::GitGraph
            && let Some(scope) = self.selected_panel_scope()
        {
            self.git_graph_snapshots.invalidate(&scope);
        }
        if matches!(
            surface,
            RightPanelSurface::Files | RightPanelSurface::File(_)
        ) {
            self.refresh_right_panel_working_tree(cx);
        }
        if let Some(terminal_id) = surface.terminal_id() {
            self.ensure_right_panel_terminal(terminal_id, cx);
        }
        // Browser views are created on the surface's first render, which has
        // the `Window` their webview must attach to.
        let index = match reusable_index {
            Some(index) => index,
            None => {
                self.right_panel_surfaces.push(surface);
                self.right_panel_surfaces.len() - 1
            }
        };
        self.right_panel_active_surface = Some(index);
        self.reveal_right_panel_tab(index);
        self.request_active_terminal_focus();
        self.request_active_browser_focus();
        self.set_right_panel_visible(true, cx);
        cx.notify();
    }

    /// Zed-style panel-button semantics: bring the surface up, or put the
    /// panel away when it is already the one showing.
    pub(super) fn toggle_right_panel_surface(
        &mut self,
        surface: RightPanelSurface,
        cx: &mut Context<Self>,
    ) {
        let showing =
            self.right_panel_visible && self.active_right_panel_surface() == Some(&surface);
        if showing {
            self.set_right_panel_visible(false, cx);
        } else {
            self.open_right_panel_surface(surface, cx);
        }
    }

    pub(super) fn open_turn_diff(&mut self, turn_id: Uuid, cx: &mut Context<Self>) {
        let Some((session_id, turn_count)) = self.selected_session().and_then(|session| {
            session
                .turns
                .iter()
                .find(|turn| turn.id == turn_id)
                .map(|turn| (session.id, turn.turn_count))
        }) else {
            return;
        };
        self.right_panel_diff_source = ReviewDiffSource::LastTurn {
            session_id,
            turn_id,
            turn_count,
        };
        self.right_panel_diff_selection.clear();
        self.right_panel_diff_snapshot = None;
        self.right_panel_diff_selected_file = None;
        self.open_right_panel_surface(RightPanelSurface::Diff, cx);
    }

    pub(super) fn open_right_panel_file(&mut self, relative_path: String, cx: &mut Context<Self>) {
        self.open_right_panel_file_at(relative_path, None, cx);
    }

    pub(super) fn open_right_panel_file_at(
        &mut self,
        relative_path: String,
        location: Option<(usize, Option<usize>)>,
        cx: &mut Context<Self>,
    ) {
        self.ensure_initial_right_panel_file_editor_width();
        if let Some(location) = location {
            if self.state.markdown_preview
                && file_highlighter_language(&relative_path) == "markdown"
            {
                self.state.markdown_preview = false;
            }
            if let Some(editor) = self.right_panel_file_editors.get_mut(&relative_path) {
                editor.pending_goto = Some(location);
                if !editor.reading {
                    let state = editor.state.clone();
                    let range = state.read(cx).line_column_range(location.0, location.1);
                    state.update(cx, |state, cx| state.select_range(range.clone(), cx));
                    self.reveal_file_search_match(&state, range.start, cx);
                }
            } else {
                self.right_panel_pending_goto = Some((relative_path.clone(), location));
            }
        }
        let Some(active) = self.right_panel_active_surface else {
            self.open_right_panel_surface(RightPanelSurface::File(relative_path), cx);
            return;
        };
        match self.right_panel_surfaces.get(active).cloned() {
            Some(RightPanelSurface::Files) => {
                let dirty_file_would_be_replaced = self
                    .right_panel_files_selected_path
                    .as_deref()
                    .is_some_and(|current_path| {
                        current_path != relative_path
                            && self.right_panel_file_is_dirty(current_path)
                    });
                if dirty_file_would_be_replaced {
                    self.open_right_panel_surface(RightPanelSurface::File(relative_path), cx);
                    return;
                }

                self.right_panel_files_selected_path = Some(relative_path);
                self.set_right_panel_visible(true, cx);
                cx.notify();
            }
            Some(RightPanelSurface::File(current_path)) => {
                if current_path == relative_path {
                    return;
                }
                if self.right_panel_file_is_dirty(&current_path) {
                    self.open_right_panel_surface(RightPanelSurface::File(relative_path), cx);
                    return;
                }

                let requested = RightPanelSurface::File(relative_path);
                if let Some(existing) =
                    reusable_surface_index(&self.right_panel_surfaces, &requested)
                {
                    self.right_panel_surfaces.remove(active);
                    let existing = if existing > active {
                        existing - 1
                    } else {
                        existing
                    };
                    self.right_panel_active_surface = Some(existing);
                    self.reveal_right_panel_tab(existing);
                } else {
                    self.right_panel_surfaces[active] = requested;
                    self.reveal_right_panel_tab(active);
                }
                self.set_right_panel_visible(true, cx);
                cx.notify();
            }
            _ => self.open_right_panel_surface(RightPanelSurface::File(relative_path), cx),
        }
    }

    fn close_right_panel_surface(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.right_panel_surfaces.len() {
            return;
        }
        if let Some(terminal_id) = self.right_panel_surfaces[index].terminal_id() {
            self.right_panel_terminals.remove(&terminal_id);
        }
        if let Some(browser_id) = self.right_panel_surfaces[index].browser_id() {
            self.right_panel_browsers.remove(&browser_id);
        }
        self.right_panel_surfaces.remove(index);
        self.right_panel_active_surface = if self.right_panel_surfaces.is_empty() {
            None
        } else {
            Some(match self.right_panel_active_surface {
                Some(active) if active > index => active - 1,
                Some(active) if active == index => index.saturating_sub(1),
                Some(active) => active.min(self.right_panel_surfaces.len() - 1),
                None => 0,
            })
        };
        if let Some(active) = self.right_panel_active_surface {
            self.reveal_right_panel_tab(active);
            self.request_active_terminal_focus();
            self.request_active_browser_focus();
        } else {
            self.right_panel_pending_tab_reveal = None;
            self.right_panel_pending_terminal_focus = None;
            self.right_panel_pending_browser_focus = None;
            self.set_right_panel_visible(false, cx);
        }
        cx.notify();
    }

    /// ⌘W converges layer by layer: the right panel's tab first, then the
    /// bottom panel's terminal, and only then the window itself.
    pub(super) fn close_window_or_right_panel_tab_action(
        &mut self,
        _: &CloseWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.right_panel_active_surface {
            self.close_right_panel_surface(active, cx);
            if self.right_panel_surfaces.is_empty() {
                let focus_handle = self.composer_focus(cx);
                window.focus(&focus_handle, cx);
            }
        } else if let Some(active) = self.bottom_panel_active_group {
            self.close_bottom_panel_group(active, cx);
            if self.bottom_panel_terminal_groups.is_empty() {
                let focus_handle = self.composer_focus(cx);
                window.focus(&focus_handle, cx);
            }
        } else {
            self.close_or_hide_window(window);
        }
    }

    pub(super) fn close_or_hide_window(&self, window: &mut Window) {
        crate::platform::hide_window(window);
    }

    /// The header's one-click surface shortcuts. They skip the open-panel
    /// then pick-a-tab round trip, and stay visible while the panel is open
    /// so they double as tab switching and as the mouse way out.
    pub(super) fn render_header_surface_buttons(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(self.render_header_surface_button(
                "header-files-button",
                RightPanelSurface::Files,
                "right_panel.files",
                crate::platform::primary_shortcut("⇧⌘E", "Ctrl+Shift+E"),
                cx,
            ))
            .child(self.render_header_surface_button(
                "header-git-graph-button",
                RightPanelSurface::GitGraph,
                "right_panel.git_graph",
                crate::platform::primary_shortcut("⇧⌘G", "Ctrl+Shift+G"),
                cx,
            ))
            .child(self.render_header_right_terminal_button(cx))
    }

    /// The header shortcut for the right panel's terminal. Unlike Files and
    /// Git it is not a single-instance surface, so the click reveals an
    /// existing terminal tab — or opens the first one — and hides the panel
    /// only when a terminal is already what is showing.
    fn render_header_right_terminal_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self.right_panel_visible
            && matches!(
                self.active_right_panel_surface(),
                Some(RightPanelSurface::Terminal(_))
            );
        let needs_attention = !active
            && self
                .right_panel_surfaces
                .iter()
                .filter_map(RightPanelSurface::terminal_id)
                .any(|id| self.right_panel_terminal_needs_attention(id, cx));
        let tooltip_label = if needs_attention {
            format!(
                "{} · {}",
                tr!("right_panel.toggle_terminal"),
                tr!("terminal.needs_attention")
            )
        } else {
            tr!("right_panel.toggle_terminal")
        };
        let focus = self.transcript_control_focus("header-right-terminal-button", cx);
        div()
            .id("header-right-terminal-button")
            .track_focus(&focus)
            .tab_index(0)
            .relative()
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| {
                style
                    .bg(theme.overlay)
                    .border_1()
                    .border_color(theme.accent)
            })
            .when(active, |element| element.bg(theme.overlay_strong))
            .when(!active, |element| {
                element.hover(|element| element.bg(theme.overlay))
            })
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon(
                "icons/terminal-square.svg",
                14.0,
                if active {
                    theme.text
                } else {
                    theme.text_tertiary
                },
            ))
            .when(needs_attention, |element| {
                element.child(
                    div()
                        .id("header-right-terminal-attention")
                        .absolute()
                        .top(px(3.0))
                        .right(px(3.0))
                        .size(px(6.0))
                        .rounded_full()
                        .bg(theme.warning),
                )
            })
            .tooltip(Tooltip::text(tooltip_label))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_right_panel_terminal(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.toggle_right_panel_terminal(cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn right_panel_terminal_needs_attention(&self, terminal_id: Uuid, cx: &App) -> bool {
        self.right_panel_terminals
            .get(&terminal_id)
            .is_some_and(|terminal| {
                let terminal = terminal.read(cx);
                !terminal.exited() && terminal.needs_attention()
            })
    }

    pub(super) fn toggle_right_panel_terminal(&mut self, cx: &mut Context<Self>) {
        match right_panel_terminal_toggle(
            self.right_panel_visible,
            &self.right_panel_surfaces,
            self.right_panel_active_surface,
        ) {
            RightPanelTerminalToggle::Hide => self.set_right_panel_visible(false, cx),
            RightPanelTerminalToggle::Show(index) => {
                self.right_panel_active_surface = Some(index);
                self.reveal_right_panel_tab(index);
                self.request_active_terminal_focus();
                self.set_right_panel_visible(true, cx);
                cx.notify();
            }
            RightPanelTerminalToggle::OpenNew => {
                self.open_right_panel_surface(RightPanelSurface::new_terminal(), cx);
            }
        }
    }

    fn render_header_surface_button(
        &self,
        id: &'static str,
        surface: RightPanelSurface,
        tooltip_key: &'static str,
        shortcut: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active =
            self.right_panel_visible && self.active_right_panel_surface() == Some(&surface);
        let focus = self.transcript_control_focus(id, cx);
        let key_surface = surface.clone();
        div()
            .id(id)
            .track_focus(&focus)
            .tab_index(0)
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| {
                style
                    .bg(theme.overlay)
                    .border_1()
                    .border_color(theme.accent)
            })
            .when(active, |element| element.bg(theme.overlay_strong))
            .when(!active, |element| {
                element.hover(|element| element.bg(theme.overlay))
            })
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon(
                surface.icon_path(),
                14.0,
                if active {
                    theme.text
                } else {
                    theme.text_tertiary
                },
            ))
            .tooltip(Tooltip::with_shortcut(tr!(tooltip_key), shortcut))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_right_panel_surface(surface.clone(), cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.toggle_right_panel_surface(key_surface.clone(), cx);
                    cx.stop_propagation();
                }
            }))
    }

    pub(super) fn render_right_panel_toggle(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active = self.right_panel_visible;
        let focus = self.transcript_control_focus("toggle-right-panel", cx);
        div()
            .id("toggle-right-panel")
            .track_focus(&focus)
            .tab_index(0)
            .w(px(26.0))
            .h(px(26.0))
            .flex_none()
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| {
                style
                    .bg(theme.overlay)
                    .border_1()
                    .border_color(theme.accent)
            })
            .when(active, |element| element.bg(theme.overlay_strong))
            .when(!active, |element| {
                element.hover(|element| element.bg(theme.overlay))
            })
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon(
                "icons/panel-right.svg",
                14.0,
                if active {
                    theme.text
                } else {
                    theme.text_tertiary
                },
            ))
            .tooltip(Tooltip::with_shortcut(
                tr!("right_panel.toggle"),
                crate::platform::primary_shortcut("⇧⌘B", "Ctrl+Shift+B"),
            ))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.set_right_panel_visible(!this.right_panel_visible, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.set_right_panel_visible(!this.right_panel_visible, cx);
                    cx.stop_propagation();
                }
            }))
    }

    pub(super) fn render_right_panel(
        &mut self,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active_terminal_id = self
            .active_right_panel_surface()
            .and_then(RightPanelSurface::terminal_id);
        if self.right_panel_pending_terminal_focus == active_terminal_id
            && let Some(terminal_id) = active_terminal_id
            && let Some(terminal) = self.right_panel_terminals.get(&terminal_id)
        {
            let focus_handle = terminal.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            self.right_panel_pending_terminal_focus = None;
        }
        let body = match self.active_right_panel_surface().cloned() {
            None => self.render_right_panel_chooser(cx).into_any_element(),
            Some(RightPanelSurface::BackgroundWork { key, .. }) => self
                .render_background_work_surface(&key, cx)
                .into_any_element(),
            Some(RightPanelSurface::Files) => self
                .render_right_panel_files(width, window, cx)
                .into_any_element(),
            Some(RightPanelSurface::Diff) => self
                .render_right_panel_diff(width, window, cx)
                .into_any_element(),
            Some(RightPanelSurface::GitGraph) => self.render_git_graph(cx),
            Some(RightPanelSurface::Terminal(terminal_id)) => self
                .right_panel_terminals
                .get(&terminal_id)
                .cloned()
                .inspect(|terminal| {
                    terminal.update(cx, |terminal, _| terminal.set_panel_width(width));
                })
                .map(IntoElement::into_any_element)
                .unwrap_or_else(|| {
                    self.render_right_panel_empty_message(
                        tr!("right_panel.terminal_unavailable"),
                        tr!("right_panel.terminal_unavailable_description"),
                        cx,
                    )
                    .into_any_element()
                }),
            Some(RightPanelSurface::File(path)) => self
                .render_right_panel_file(path, width, window, cx)
                .into_any_element(),
            Some(RightPanelSurface::Browser(browser_id)) => {
                let browser = self.ensure_right_panel_browser(browser_id, window, cx);
                if self
                    .right_panel_pending_browser_focus
                    .take_if(|pending| *pending == browser_id)
                    .is_some()
                {
                    browser.update(cx, |view, cx| view.focus_default(window, cx));
                }
                browser.into_any_element()
            }
        };

        div()
            .id("right-panel")
            .w(px(width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .min_w_0()
            .border_l_1()
            .border_color(theme.border_strong)
            .bg(theme.surface)
            .relative()
            .child(self.render_right_panel_header(window, cx))
            .child(body)
            .child(self.render_panel_resize_handle(
                "right-panel-resize-handle",
                PanelResizeTarget::RightPanel,
                cx,
            ))
    }

    fn ensure_right_panel_browser(
        &mut self,
        browser_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<crate::browser::BrowserView> {
        if let Some(browser) = self.right_panel_browsers.get(&browser_id) {
            return browser.clone();
        }
        let browser = cx.new(|cx| crate::browser::BrowserView::new(window, cx));
        // Tab titles and toolbar state live on the browser entity; the panel
        // chrome re-renders when they move.
        cx.observe(&browser, |_, _, cx| cx.notify()).detach();
        self.right_panel_browsers
            .insert(browser_id, browser.clone());
        browser
    }

    /// Drop browser views whose tab no longer exists in any session.
    pub(super) fn retain_right_panel_browsers(&mut self) {
        let retained_browser_ids = self
            .right_panel_surfaces
            .iter()
            .filter_map(RightPanelSurface::browser_id)
            .chain(
                self.right_panel_workspace_states
                    .values()
                    .flat_map(|state| {
                        state
                            .surfaces
                            .iter()
                            .filter_map(RightPanelSurface::browser_id)
                    }),
            )
            .collect::<HashSet<_>>();
        self.right_panel_browsers
            .retain(|browser_id, _| retained_browser_ids.contains(browser_id));
    }

    /// Whether any GPUI overlay that could float above the right panel is
    /// open. The native webview always draws over GPUI, so while this holds
    /// the live page swaps for a frozen snapshot.
    fn any_overlay_open(&self, cx: &App) -> bool {
        self.menus.borrow().values().any(ContextMenuHandle::is_open)
            || self.selected_runtime().is_some_and(|runtime| {
                runtime.computer_use_previews.iter().any(|preview| {
                    preview.visible
                        && preview.target.is_some()
                        && preview.phase != ComputerUsePhase::AwaitingApproval
                })
            })
            || self.command_palette.is_open()
            || self.task_switcher.is_open()
            || self.commit_dialog.is_some()
            || self.image_preview.is_some()
            || self.composer.read(cx).context_menu_open(cx)
            || self
                .right_panel_browsers
                .values()
                .any(|browser| browser.read(cx).overlay_open(cx))
    }

    /// Once per frame, from the very top of the app's render: push down to
    /// every browser whether its native view belongs on screen. This is the
    /// single authority — tab switches, panel toggles, session switches, the
    /// settings page and overlay menus all funnel through here, so a webview
    /// can never linger over unrelated UI.
    pub(super) fn sync_browser_webviews(&mut self, cx: &mut Context<Self>) {
        if self.right_panel_browsers.is_empty() {
            return;
        }
        // With the scene overlay compositing GPUI's deferred draws above
        // native views, open menus never occlude the webview — the snapshot
        // swap is purely the fallback for a window where enabling it failed.
        let overlay_open = !self.scene_overlay_enabled && self.any_overlay_open(cx);
        // A webview composites above the GPUI scene, so the panel's clip does
        // not apply to it: shown mid-slide it would hang over the transcript
        // at full width. Keep it down until the panel has finished moving.
        let active_browser = if self.settings_page.is_none()
            && self.right_panel_visible
            && self.right_panel_slide.is_none()
        {
            self.active_right_panel_surface()
                .and_then(RightPanelSurface::browser_id)
        } else {
            None
        };
        for (browser_id, browser) in &self.right_panel_browsers {
            let surface_visible = active_browser == Some(*browser_id);
            browser.update(cx, |view, cx| {
                view.sync_native_state(surface_visible, overlay_open, cx);
            });
        }
    }

    fn ensure_right_panel_terminal(&mut self, terminal_id: Uuid, cx: &mut Context<Self>) {
        let Some(working_directory) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            self.right_panel_terminals.remove(&terminal_id);
            return;
        };
        let matches_project = self
            .right_panel_terminals
            .get(&terminal_id)
            .is_some_and(|terminal| terminal.read(cx).working_directory() == working_directory);
        if !matches_project {
            // A desktop PTY would interpret a remote daemon's working
            // directory on the wrong machine, so remote windows run the shell
            // on the daemon host.
            let launcher = if self.selected_daemon().is_remote() {
                crate::terminal::TerminalLauncher::Daemon(self.selected_daemon().clone())
            } else {
                crate::terminal::TerminalLauncher::Local
            };
            let terminal = cx.new(|cx| TerminalView::new(working_directory.clone(), launcher, cx));
            Self::show_terminal_notices(&terminal, cx);
            self.right_panel_terminals.insert(terminal_id, terminal);
        }
    }

    pub(super) fn ensure_right_panel_terminals(&mut self, cx: &mut Context<Self>) {
        let active_terminal_ids = self
            .right_panel_surfaces
            .iter()
            .filter_map(RightPanelSurface::terminal_id)
            .collect::<Vec<_>>();
        let retained_terminal_ids = active_terminal_ids
            .iter()
            .copied()
            .chain(
                self.right_panel_workspace_states
                    .values()
                    .flat_map(|state| {
                        state
                            .surfaces
                            .iter()
                            .filter_map(RightPanelSurface::terminal_id)
                    }),
            )
            .collect::<HashSet<_>>();
        self.right_panel_terminals
            .retain(|terminal_id, _| retained_terminal_ids.contains(terminal_id));
        for terminal_id in active_terminal_ids {
            self.ensure_right_panel_terminal(terminal_id, cx);
        }
    }

    fn render_right_panel_header(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let active_surface = self.right_panel_active_surface;
        let mut tabs = div()
            .id("right-panel-tabs")
            .h_full()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap(px(4.0))
            .overflow_x_scroll()
            .track_scroll(&self.right_panel_tabs_scroll_handle);
        for (index, surface) in self.right_panel_surfaces.iter().cloned().enumerate() {
            let active = active_surface == Some(index);
            let dirty = self.right_panel_surface_is_dirty(&surface);
            let needs_attention = surface
                .terminal_id()
                .is_some_and(|id| self.right_panel_terminal_needs_attention(id, cx));
            let label = SharedString::from(match &surface {
                // Browser tabs read like browser tabs: the page title once
                // known, the address until then.
                RightPanelSurface::Browser(browser_id) => self
                    .right_panel_browsers
                    .get(browser_id)
                    .and_then(|browser| browser.read(cx).tab_label())
                    .unwrap_or_else(|| surface.label()),
                _ => {
                    right_panel_tab_label(&surface, self.right_panel_files_selected_path.as_deref())
                }
            });
            let icon_path =
                right_panel_tab_icon(&surface, self.right_panel_files_selected_path.as_deref());
            let uses_file_icon = matches!(&surface, RightPanelSurface::File(_))
                || matches!(&surface, RightPanelSurface::Files)
                    && self.right_panel_files_selected_path.is_some();
            let activate_weak = cx.entity().downgrade();
            let close_weak = cx.entity().downgrade();
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("right-panel-tab-{index}")))
                    .h(px(28.0))
                    .min_w(px(100.0))
                    .max_w(px(176.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_default()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .when(active, |element| element.bg(theme.overlay_strong))
                    .when(!active, |element| {
                        element.hover(|element| element.bg(theme.overlay))
                    })
                    .child(if uses_file_icon {
                        file_icon(icon_path, 13.0).into_any_element()
                    } else {
                        icon(icon_path, 13.0, theme.text_secondary).into_any_element()
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(sp(12.5))
                            .text_color(if active {
                                theme.text
                            } else {
                                theme.text_secondary
                            })
                            .child(label),
                    )
                    .when(dirty, |element| {
                        element.child(
                            div()
                                .id(SharedString::from(format!("right-panel-tab-dirty-{index}")))
                                .size(px(7.0))
                                .flex_none()
                                .rounded_full()
                                .bg(theme.warning)
                                .tooltip(|window, cx| {
                                    Tooltip::new(tr!(
                                        "files.unsaved_changes",
                                        shortcut =
                                            crate::platform::primary_shortcut("⌘S", "Ctrl+S")
                                    ))
                                    .build(window, cx)
                                }),
                        )
                    })
                    .when(needs_attention, |element| {
                        element.child(
                            div()
                                .id(SharedString::from(format!(
                                    "right-panel-tab-attention-{index}"
                                )))
                                .size(px(10.0))
                                .flex_none()
                                .rounded_full()
                                .bg(theme.warning)
                                .border_2()
                                .border_color(theme.warning.opacity(0.35))
                                .tooltip(|window, cx| {
                                    Tooltip::new(tr!("terminal.needs_attention")).build(window, cx)
                                }),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("close-right-panel-tab-{index}")))
                            .w(px(16.0))
                            .h(px(16.0))
                            .rounded(px(4.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(|element| element.bg(theme.overlay_strong))
                            .child(icon("icons/x.svg", 10.0, theme.text_tertiary))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let _ = close_weak.update(cx, |this, cx| {
                                    this.close_right_panel_surface(index, cx);
                                });
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = activate_weak.update(cx, |this, cx| {
                            this.right_panel_active_surface = Some(index);
                            this.reveal_right_panel_tab(index);
                            this.request_active_terminal_focus();
                            cx.notify();
                        });
                    }),
            );
        }
        tabs = tabs.child(div().w(px(TAB_SCROLL_FADE_WIDTH)).h(px(1.0)).flex_none());

        let mut header = div()
            .id("right-panel-header")
            .h(px(48.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(10.0))
            .pr(px(14.0))
            .child(
                div()
                    .relative()
                    .h_full()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(tabs)
                    .when_some(self.right_panel_pending_tab_reveal, |element, tab_index| {
                        element.child(tab_scroll_reveal_guard(
                            self.right_panel_tabs_scroll_handle.clone(),
                            tab_index,
                            cx.entity().downgrade(),
                            PanelTabs::RightPanel,
                        ))
                    })
                    .child(tab_scroll_fade(
                        self.right_panel_tabs_scroll_handle.clone(),
                        TabScrollFadeSide::Left,
                        theme.surface,
                    ))
                    .child(tab_scroll_fade(
                        self.right_panel_tabs_scroll_handle.clone(),
                        TabScrollFadeSide::Right,
                        theme.surface,
                    )),
            );

        if !self.right_panel_surfaces.is_empty() {
            let weak = cx.entity().downgrade();
            let existing_surfaces = self.right_panel_surfaces.clone();
            let options = [
                RightPanelSurface::new_browser(),
                RightPanelSurface::new_terminal(),
                RightPanelSurface::Files,
                RightPanelSurface::Diff,
                RightPanelSurface::GitGraph,
            ];
            let handle = self.menu_handle("add-right-panel-surface", cx);
            header = header.child(
                div()
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(dropdown_menu(
                        icon_button("add-right-panel-surface", "icons/plus.svg", theme),
                        "add-right-panel-surface-menu",
                        &handle,
                        MenuAlign::BelowRight,
                        move |_| {
                            options
                                .clone()
                                .into_iter()
                                .map(|surface| {
                                    let weak = weak.clone();
                                    let open_surface = surface.clone();
                                    let already_open =
                                        reusable_surface_index(&existing_surfaces, &surface)
                                            .is_some();
                                    MenuItem::new(surface.label(), move |_, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            this.open_right_panel_surface(open_surface.clone(), cx);
                                        });
                                    })
                                    .icon(surface.icon_path())
                                    .selected(already_open)
                                })
                                .collect()
                        },
                    )),
            );
        }

        self.window_drag_region(
            header.child(self.render_right_panel_toggle(cx)).children(
                self.render_client_window_controls(
                    super::window_chrome::WindowControlSide::Right,
                    window,
                    cx,
                ),
            ),
            cx,
        )
    }

    fn render_right_panel_chooser(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = Theme::current(cx);
        div()
            .id("right-panel-chooser")
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .px(px(20.0))
            .pb(px(32.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(420.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_size(sp(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("right_panel.open_surface")),
                    )
                    .child(
                        div()
                            .mt(px(5.0))
                            .text_size(sp(12.5))
                            .text_color(theme.text_tertiary)
                            .child(tr!("right_panel.choose_surface")),
                    )
                    .child(
                        div()
                            .mt(px(18.0))
                            .w_full()
                            .flex()
                            .gap(px(8.0))
                            .child(self.render_right_panel_card(
                                RightPanelSurface::new_browser(),
                                tr!("right_panel.browser_description"),
                                cx,
                            ))
                            .child(self.render_right_panel_card(
                                RightPanelSurface::new_terminal(),
                                tr!("right_panel.terminal_description"),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .mt(px(8.0))
                            .w_full()
                            .flex()
                            .gap(px(8.0))
                            .child(self.render_right_panel_card(
                                RightPanelSurface::Files,
                                tr!("right_panel.files_description"),
                                cx,
                            ))
                            .child(self.render_right_panel_card(
                                RightPanelSurface::Diff,
                                tr!("right_panel.diff_description"),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .mt(px(8.0))
                            .w_full()
                            .flex()
                            .gap(px(8.0))
                            .child(self.render_right_panel_card(
                                RightPanelSurface::GitGraph,
                                tr!("right_panel.git_graph_description"),
                                cx,
                            ))
                            .child(self.render_right_panel_card_placeholder()),
                    ),
            )
    }

    fn render_right_panel_card_placeholder(&self) -> Stateful<Div> {
        div()
            .id("right-panel-card-placeholder")
            .h(px(112.0))
            .flex_1()
            .min_w_0()
            .p(px(14.0))
            .border_1()
            .border_color(gpui::transparent_black())
            .invisible()
    }

    fn render_right_panel_card(
        &self,
        surface: RightPanelSurface,
        description: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let icon_path = surface.icon_path();
        let label = surface.label();
        div()
            .id(SharedString::from(format!(
                "right-panel-card-{}",
                label.to_lowercase()
            )))
            .h(px(112.0))
            .flex_1()
            .min_w_0()
            .p(px(14.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.composer)
            .flex()
            .flex_col()
            .items_start()
            .cursor_default()
            .hover(|element| element.bg(theme.raised).border_color(theme.text_ghost))
            .active(|element| element.bg(theme.overlay_strong))
            .child(icon(icon_path, 18.0, theme.text_secondary))
            .child(
                div()
                    .mt(px(12.0))
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(label),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .text_size(sp(12.5))
                    .line_height(sp(15.0))
                    .text_color(theme.text_tertiary)
                    .whitespace_normal()
                    .line_clamp(2)
                    .text_overflow(gpui::TextOverflow::Truncate("...".into()))
                    .child(description),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_right_panel_surface(surface.clone(), cx);
            }))
    }

    fn render_right_panel_files(
        &mut self,
        panel_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        if let Some(relative_path) = self.right_panel_files_selected_path.clone() {
            self.render_right_panel_file(relative_path, panel_width, window, cx)
        } else {
            self.render_right_panel_working_tree(None, cx)
        }
    }

    fn render_right_panel_working_tree(
        &self,
        selected_path: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let Some(project) = self.selected_project() else {
            return self.render_right_panel_empty_message(
                tr!("files.no_project_open"),
                tr!("files.no_project_open_description"),
                cx,
            );
        };
        let project_name = project.display_name();
        // Read only. The walk is filesystem I/O, so it happens in
        // `refresh_right_panel_working_tree`, never in a frame.
        let entries = self.right_panel_working_tree.clone();

        let mut list = div().flex().flex_col().py(px(6.0));
        if let Some(RightPanelFileRename::Create {
            parent_relative_path,
            depth,
            is_dir,
        }) = &self.right_panel_file_rename
            && parent_relative_path.is_empty()
        {
            list = list
                .child(self.render_right_panel_file_create_row("", *depth, *is_dir, &theme, cx));
        }
        for entry in entries {
            let relative_path = entry.relative_path.clone();
            let absolute_path = entry.absolute_path.clone();
            let name = entry.name.clone();
            let is_dir = entry.is_dir;
            let entry_depth = entry.depth;
            let selected = selected_path == Some(relative_path.as_str());
            let renaming = matches!(
                self.right_panel_file_rename.as_ref(),
                Some(RightPanelFileRename::Rename {
                    relative_path: target,
                    ..
                }) if target == &relative_path
            );
            let rename_input = renaming.then(|| self.right_panel_file_rename_input.clone());
            let waku = cx.entity().downgrade();
            let menu = self.menu_handle(
                SharedString::from(format!("right-panel-file-menu-{relative_path}")),
                cx,
            );
            let keyboard_menu = menu.clone();
            let key_relative_path = relative_path.clone();
            let key_absolute_path = absolute_path.clone();
            let click_relative_path = relative_path.clone();
            let click_absolute_path = absolute_path.clone();
            let menu_relative_path = relative_path.clone();
            let menu_absolute_path = absolute_path.clone();
            let row_focus = menu.trigger_focus_handle().clone();
            let row = div()
                .id(SharedString::from(format!(
                    "right-panel-file-{relative_path}"
                )))
                .h(px(30.0))
                .mx(px(8.0))
                .pl(px(8.0 + entry.depth as f32 * 16.0))
                .pr(px(8.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .cursor_default()
                .when(selected, |element| element.bg(theme.overlay_strong))
                .hover(|element| element.bg(theme.overlay))
                .child(if is_dir {
                    icon(
                        if entry.expanded {
                            "icons/chevron-down.svg"
                        } else {
                            "icons/chevron-right.svg"
                        },
                        10.0,
                        theme.text_ghost,
                    )
                    .into_any_element()
                } else {
                    div().w(px(10.0)).h(px(10.0)).flex_none().into_any_element()
                })
                .when_some(entry.file_icon, |element, file_icon_path| {
                    element.child(file_icon(file_icon_path, 14.0))
                })
                .child(if let Some(rename_input) = rename_input {
                    div()
                        .id(SharedString::from(format!(
                            "right-panel-file-rename-{relative_path}"
                        )))
                        .key_context(FILE_RENAME_PARENT_CONTEXT)
                        .on_action(cx.listener(
                            |this, _: &CancelRightPanelFileRename, window, cx| {
                                this.cancel_right_panel_file_rename(window, cx);
                            },
                        ))
                        .h(px(18.0))
                        .flex_1()
                        .min_w_0()
                        .px(px(4.0))
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(theme.accent)
                        .bg(theme.inset)
                        .flex()
                        .items_center()
                        .text_size(sp(12.0))
                        .text_color(theme.text)
                        .child(rename_input)
                        .into_any_element()
                } else {
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(sp(12.5))
                        .text_color(theme.text_secondary)
                        .child(name.clone())
                        .into_any_element()
                })
                .when(!renaming, |element| {
                    element
                        .track_focus(&row_focus)
                        .tab_index(0)
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            let key = event.keystroke.key.as_str();
                            if matches!(key, "enter" | "space") {
                                if is_dir {
                                    if !this.right_panel_expanded_paths.remove(&key_absolute_path) {
                                        this.right_panel_expanded_paths
                                            .insert(key_absolute_path.clone());
                                    }
                                    this.refresh_right_panel_working_tree(cx);
                                    cx.notify();
                                } else {
                                    this.open_right_panel_file(key_relative_path.clone(), cx);
                                }
                                cx.stop_propagation();
                            } else if key == "f10" && event.keystroke.modifiers.shift {
                                keyboard_menu.open_context_menu(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if is_dir {
                                if !this.right_panel_expanded_paths.remove(&click_absolute_path) {
                                    this.right_panel_expanded_paths
                                        .insert(click_absolute_path.clone());
                                }
                                this.refresh_right_panel_working_tree(cx);
                                cx.notify();
                            } else {
                                this.open_right_panel_file(click_relative_path.clone(), cx);
                            }
                        }))
                });
            let row_relative = relative_path.clone();
            let row = if renaming {
                div()
                    .w_full()
                    .child(row)
                    .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                        if matches!(
                            this.right_panel_file_rename.as_ref(),
                            Some(RightPanelFileRename::Rename {
                                relative_path: target,
                                ..
                            }) if target == &row_relative
                        ) {
                            this.commit_right_panel_file_rename(cx);
                        }
                    }))
                    .into_any_element()
            } else {
                context_menu(
                    div().w_full().child(row),
                    SharedString::from(format!("right-panel-file-menu-{relative_path}")),
                    &menu,
                    move |_| {
                        let new_file_waku = waku.clone();
                        let new_folder_waku = waku.clone();
                        let copy_rel_waku = waku.clone();
                        let copy_abs_waku = waku.clone();
                        let download_waku = waku.clone();
                        let rename_waku = waku.clone();
                        let delete_waku = waku.clone();
                        let parent_for_create = if is_dir {
                            menu_relative_path.clone()
                        } else {
                            menu_relative_path
                                .rfind(['/', '\\'])
                                .map(|idx| menu_relative_path[..idx].to_owned())
                                .unwrap_or_default()
                        };
                        let create_depth = if is_dir { entry_depth + 1 } else { entry_depth };
                        let expand_dir = is_dir.then(|| menu_absolute_path.clone());
                        let mut items = Vec::new();
                        {
                            let parent = parent_for_create.clone();
                            let expand = expand_dir.clone();
                            items.push(
                                MenuItem::new(tr!("files.new_file"), move |window, cx| {
                                    let _ = new_file_waku.update(cx, |waku, cx| {
                                        waku.begin_right_panel_file_create(
                                            parent.clone(),
                                            create_depth,
                                            false,
                                            expand.clone(),
                                            window,
                                            cx,
                                        )
                                    });
                                })
                                .icon("icons/file.svg"),
                            );
                        }
                        {
                            let parent = parent_for_create;
                            let expand = expand_dir;
                            items.push(
                                MenuItem::new(tr!("files.new_folder"), move |window, cx| {
                                    let _ = new_folder_waku.update(cx, |waku, cx| {
                                        waku.begin_right_panel_file_create(
                                            parent.clone(),
                                            create_depth,
                                            true,
                                            expand.clone(),
                                            window,
                                            cx,
                                        )
                                    });
                                })
                                .icon("icons/folder-new.svg"),
                            );
                        }
                        items.push(MenuItem::Separator);
                        {
                            let rel = menu_relative_path.clone();
                            items.push(
                                MenuItem::new(tr!("files.copy_relative_path"), move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(rel.clone()));
                                    let _ = copy_rel_waku.update(cx, |waku, cx| {
                                        waku.show_success_toast(tr!("files.path_copied"));
                                        cx.notify();
                                    });
                                })
                                .icon("icons/copy.svg"),
                            );
                        }
                        {
                            let abs = menu_absolute_path.to_string_lossy().into_owned();
                            items.push(
                                MenuItem::new(tr!("files.copy_path"), move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(abs.clone()));
                                    let _ = copy_abs_waku.update(cx, |waku, cx| {
                                        waku.show_success_toast(tr!("files.path_copied"));
                                        cx.notify();
                                    });
                                })
                                .icon("icons/copy.svg"),
                            );
                        }
                        items.push(MenuItem::Separator);
                        if !is_dir {
                            let relative_path = menu_relative_path.clone();
                            let item_name = name.clone();
                            items.push(
                                MenuItem::new(tr!("files.download"), move |_, cx| {
                                    let _ = download_waku.update(cx, |waku, cx| {
                                        waku.download_right_panel_file(
                                            relative_path.clone(),
                                            item_name.clone(),
                                            cx,
                                        )
                                    });
                                })
                                .icon("icons/download.svg"),
                            );
                            items.push(MenuItem::Separator);
                        }
                        let rename_relative_path = menu_relative_path.clone();
                        items.push(
                            MenuItem::new(tr!("common.rename"), move |window, cx| {
                                let _ = rename_waku.update(cx, |waku, cx| {
                                    waku.begin_right_panel_file_rename(
                                        rename_relative_path.clone(),
                                        window,
                                        cx,
                                    )
                                });
                            })
                            .icon("icons/pencil.svg"),
                        );
                        let delete_relative_path = menu_relative_path.clone();
                        let delete_name = name.clone();
                        items.push(
                            MenuItem::new(tr!("files.delete"), move |window, cx| {
                                let _ = delete_waku.update(cx, |waku, cx| {
                                    waku.request_right_panel_file_delete(
                                        delete_relative_path.clone(),
                                        delete_name.clone(),
                                        is_dir,
                                        window,
                                        cx,
                                    )
                                });
                            })
                            .icon("icons/trash.svg"),
                        );
                        items
                    },
                )
                .into_any_element()
            };
            list = list.child(row);
            if let Some(RightPanelFileRename::Create {
                parent_relative_path,
                depth,
                is_dir: create_is_dir,
            }) = &self.right_panel_file_rename
                && is_dir
                && parent_relative_path == &relative_path
            {
                list = list.child(self.render_right_panel_file_create_row(
                    parent_relative_path,
                    *depth,
                    *create_is_dir,
                    &theme,
                    cx,
                ));
            }
        }

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(42.0))
                    .flex_none()
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(icon("icons/folder.svg", 13.0, theme.text_tertiary))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_secondary)
                            .child(project_name),
                    )
                    .child(
                        icon_button("files-new-file", "icons/file.svg", theme)
                            .tab_index(0)
                            .focus_visible(|style| style.border_1().border_color(theme.accent))
                            .tooltip(Tooltip::text(tr!("files.new_file")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.begin_right_panel_file_create(
                                    String::new(),
                                    0,
                                    false,
                                    None,
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        icon_button("files-new-folder", "icons/folder-new.svg", theme)
                            .tab_index(0)
                            .focus_visible(|style| style.border_1().border_color(theme.accent))
                            .tooltip(Tooltip::text(tr!("files.new_folder")))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.begin_right_panel_file_create(
                                    String::new(),
                                    0,
                                    true,
                                    None,
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div()
                            .id("right-panel-files-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.right_panel_files_scroll_handle)
                            .child(list),
                    )
                    .child(scrollbar::vertical(
                        &self.right_panel_files_scroll_handle,
                        &self.right_panel_files_scrollbar,
                    )),
            )
    }

    fn render_right_panel_file_create_row(
        &self,
        parent_relative_path: &str,
        depth: usize,
        is_dir: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let create_input = self.right_panel_file_rename_input.clone();
        let parent_owned = parent_relative_path.to_owned();
        let row = div()
            .id(SharedString::from(format!(
                "right-panel-file-create-{parent_relative_path}"
            )))
            .h(px(30.0))
            .mx(px(8.0))
            .pl(px(8.0 + depth as f32 * 16.0))
            .pr(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(icon(
                if is_dir {
                    "icons/folder-new.svg"
                } else {
                    "icons/file.svg"
                },
                12.0,
                theme.text_tertiary,
            ))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "right-panel-file-create-field-{parent_relative_path}"
                    )))
                    .key_context(FILE_RENAME_PARENT_CONTEXT)
                    .on_action(
                        cx.listener(|this, _: &CancelRightPanelFileRename, window, cx| {
                            this.cancel_right_panel_file_rename(window, cx);
                        }),
                    )
                    .h(px(18.0))
                    .flex_1()
                    .min_w_0()
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.accent)
                    .bg(theme.inset)
                    .flex()
                    .items_center()
                    .text_size(sp(12.0))
                    .text_color(theme.text)
                    .child(create_input),
            );
        div()
            .w_full()
            .child(row)
            .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                if matches!(
                    this.right_panel_file_rename.as_ref(),
                    Some(RightPanelFileRename::Create {
                        parent_relative_path: target,
                        ..
                    }) if target == &parent_owned
                ) {
                    this.commit_right_panel_file_rename(cx);
                }
            }))
            .into_any_element()
    }

    /// The delete confirm dialog: a deferred scrim layer, so a destructive
    /// remote delete always needs a second explicit action.
    pub(super) fn render_right_panel_delete_confirm(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let confirm_state = self.right_panel_delete_confirm.clone()?;
        let theme = Theme::current(cx);

        let cancel = div()
            .id("right-panel-delete-cancel")
            .track_focus(&confirm_state.cancel_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .h(px(32.0))
            .px(px(14.0))
            .rounded(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_secondary)
            .bg(theme.overlay)
            .hover(|element| element.bg(theme.overlay_strong))
            .child(tr_cow!("common.cancel"))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.right_panel_delete_confirm = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.right_panel_delete_confirm = None;
                cx.notify();
            }));
        let confirm = div()
            .id("right-panel-delete-confirm")
            .track_focus(&confirm_state.confirm_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.danger))
            .h(px(32.0))
            .px(px(14.0))
            .rounded(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.danger)
            .bg(theme.danger_soft)
            .hover(|element| element.bg(theme.danger.opacity(0.16)))
            .child(icon("icons/trash.svg", 13.0, theme.danger))
            .child(tr_cow!("files.delete"))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.confirm_right_panel_file_delete_action(
                        &ConfirmRightPanelFileDelete,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.confirm_right_panel_file_delete_action(
                    &ConfirmRightPanelFileDelete,
                    window,
                    cx,
                );
            }));

        let message = if confirm_state.is_dir {
            tr!(
                "files.delete_confirm_message_dir",
                name = confirm_state.name
            )
        } else {
            tr!("files.delete_confirm_message", name = confirm_state.name)
        };
        let card = div()
            .id("right-panel-delete-confirm-card")
            .key_context(FILE_DELETE_DIALOG_CONTEXT)
            .on_action(
                cx.listener(|waku, _: &ConfirmRightPanelFileDelete, window, cx| {
                    waku.confirm_right_panel_file_delete_action(
                        &ConfirmRightPanelFileDelete,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(
                cx.listener(|waku, _: &DismissRightPanelFileDelete, window, cx| {
                    waku.dismiss_right_panel_file_delete_action(
                        &DismissRightPanelFileDelete,
                        window,
                        cx,
                    );
                }),
            )
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(400.0))
            .overflow_hidden()
            .rounded(px(18.0))
            .bg(theme.composer)
            .shadow_xl()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(48.0))
                    .px(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .text_size(sp(14.0))
                    .text_color(theme.text)
                    .child(icon("icons/alert.svg", 15.0, theme.danger))
                    .child(tr_cow!("files.delete_confirm_title")),
            )
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(14.0))
                    .text_size(sp(12.5))
                    .line_height(sp(17.0))
                    .text_color(theme.text_secondary)
                    .child(message),
            )
            .child(div().mx(px(16.0)).h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .p(px(12.0))
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(cancel)
                    .child(confirm),
            );

        let scrim = if theme.is_dark {
            gpui::hsla(0.0, 0.0, 0.0, 0.34)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.16)
        };
        let layer = div()
            .id("right-panel-delete-confirm-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(scrim)
            .p(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|waku, _, window, cx| {
                    waku.dismiss_right_panel_file_delete_action(
                        &DismissRightPanelFileDelete,
                        window,
                        cx,
                    );
                }),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }

    fn render_right_panel_file(
        &mut self,
        relative_path: String,
        panel_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let file_tree_width = fitted_file_tree_width(panel_width, self.right_panel_file_tree_width);
        let (editor_state, writable, _) =
            self.ensure_right_panel_file_editor(&relative_path, window, cx);

        // Markdown files carry the global source/preview toggle; every other
        // language always shows source.
        let is_markdown = file_highlighter_language(&relative_path) == "markdown";
        let preview = is_markdown && self.state.markdown_preview;
        let body = if preview {
            self.render_file_markdown_preview(&relative_path, &editor_state, cx)
        } else {
            self.render_file_editor_body(
                &relative_path,
                &editor_state,
                panel_width - file_tree_width,
                writable,
                window,
                cx,
            )
        };
        let preview_toggle = is_markdown.then(|| {
            let focus = self.transcript_control_focus("file-markdown-preview-toggle", cx);
            let (icon_path, label) = if preview {
                ("icons/pencil.svg", tr!("files.edit_markdown_source"))
            } else {
                ("icons/eye.svg", tr!("files.preview_markdown"))
            };
            div()
                .id("file-markdown-preview-toggle")
                .track_focus(&focus)
                .tab_index(0)
                .size(px(26.0))
                .rounded(px(7.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .cursor_default()
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .hover(|style| style.bg(theme.overlay))
                .child(icon(icon_path, 12.0, theme.text_tertiary))
                .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_markdown_preview(cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.toggle_markdown_preview(cx);
                        cx.stop_propagation();
                    }
                }))
        });

        let editor = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(42.0))
                    .flex_none()
                    .px(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(file_icon(file_icon_for_path(&relative_path), 13.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(sp(12.5))
                            .text_color(theme.text_secondary)
                            .child(relative_path.clone()),
                    )
                    .children(preview_toggle),
            )
            .child(body);

        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .child(editor)
            .child(
                div()
                    .w(px(file_tree_width))
                    .min_w(px(FILE_TREE_MIN_WIDTH))
                    .h_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .relative()
                    .border_l_1()
                    .border_color(theme.border_strong)
                    .child(self.render_right_panel_working_tree(Some(&relative_path), cx))
                    .child(self.render_panel_resize_handle(
                        "right-panel-file-tree-resize-handle",
                        PanelResizeTarget::FileTree,
                        cx,
                    )),
            )
    }

    fn ensure_right_panel_file_editor(
        &mut self,
        relative_path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<TextInput>, bool, bool) {
        if let Some(editor) = self.right_panel_file_editors.get(relative_path) {
            return (editor.state.clone(), editor.writable, editor.dirty);
        }

        // Reached from `render`, so the file cannot be read here. The editor
        // starts empty and locked, and `read_right_panel_file_into_editor`
        // fills it in from the background executor a frame or two later.
        let language = file_highlighter_language(relative_path);
        let state = cx.new(|cx| {
            TextInput::new(window, cx)
                .multi_line()
                .syntax(Some(language))
                .read_only(true)
        });

        let pending_goto = self
            .right_panel_pending_goto
            .as_ref()
            .filter(|(path, _)| path == relative_path)
            .map(|(_, location)| *location);
        if pending_goto.is_some() {
            self.right_panel_pending_goto = None;
        }
        self.right_panel_file_editors.insert(
            relative_path.to_owned(),
            RightPanelFileEditor {
                state: state.clone(),
                disk_content: String::new(),
                writable: false,
                dirty: false,
                reading: false,
                read_epoch: 0,
                pending_goto,
            },
        );

        // Dirty tracking follows content edits. Observing raw notifies would
        // also fire for caret blinks and selection drags, cloning the whole
        // file's text for each one.
        let subscribed_path = relative_path.to_owned();
        cx.subscribe(
            &state,
            move |this: &mut Self, state, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Edited) {
                    return;
                }
                let value = state.read(cx).content().to_owned();
                if let Some(editor) = this
                    .right_panel_file_editors
                    .get_mut(subscribed_path.as_str())
                {
                    let dirty = editor.writable && value != editor.disk_content;
                    if editor.dirty != dirty {
                        editor.dirty = dirty;
                        cx.notify();
                    }
                }
                // Any content change — typing, a replace, a reload from disk —
                // moves the text out from under an open find's match list.
                this.refresh_file_search_for_edit(subscribed_path.as_str(), cx);
            },
        )
        .detach();

        let focused_path = relative_path.to_owned();
        cx.subscribe(&state, move |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Focus) {
                this.reload_right_panel_file_if_clean(focused_path.as_str(), cx);
            }
        })
        .detach();

        self.read_right_panel_file_into_editor(relative_path.to_owned(), cx);
        (state, false, false)
    }

    /// Reads a file into its editor off the UI thread.
    ///
    /// One `read_to_string` of an arbitrarily large file — hundreds of frames
    /// for a big one — so it never runs in a frame. The editor keeps whatever
    /// it is already showing until the read lands.
    ///
    /// The result is applied only if the same session is still selected and the
    /// editor is still the one that asked, so a read started before a project
    /// or session switch cannot write another workspace's text into the view.
    fn read_right_panel_file_into_editor(&mut self, relative_path: String, cx: &mut Context<Self>) {
        let project_path = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf);
        let (Some(project_path), Some(session_id)) = (project_path, self.state.selected_session)
        else {
            // Nothing to read from. Say so in the editor rather than leaving it
            // looking like an empty file.
            if let Some(editor) = self.right_panel_file_editors.get_mut(&relative_path) {
                editor.reading = false;
                editor.disk_content = tr!("files.no_project_is_open");
                editor.writable = false;
                let state = editor.state.clone();
                let content = editor.disk_content.clone();
                state.update(cx, |state, cx| state.set_content(content, cx));
            }
            return;
        };
        let Some(editor) = self.right_panel_file_editors.get_mut(&relative_path) else {
            return;
        };
        // A second asker would only duplicate the read and race to apply it.
        if editor.reading {
            return;
        }
        editor.reading = true;
        editor.read_epoch += 1;
        let epoch = editor.read_epoch;
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());

        cx.spawn(async move |waku, cx| {
            let read = cx
                .background_executor()
                .spawn({
                    let project_path = project_path.clone();
                    let relative_path = relative_path.clone();
                    async move { read_right_panel_file(&workspace, &project_path, &relative_path) }
                })
                .await;
            waku.update(cx, |waku, cx| {
                if waku.state.selected_session != Some(session_id)
                    || waku
                        .selected_workspace_path()
                        .is_none_or(|path| path != project_path)
                {
                    // The editor moved into another session's stored state, or
                    // the project changed. Clear the flag so a later reload can
                    // ask again, and drop the text.
                    if let Some(editor) = waku.right_panel_file_editors.get_mut(&relative_path) {
                        editor.reading = false;
                    }
                    return;
                }
                let (content, writable) = read;
                let Some(editor) = waku.right_panel_file_editors.get_mut(&relative_path) else {
                    return;
                };
                // A save landed while the read was in flight, so this text
                // describes the file as it was before that save.
                if editor.read_epoch != epoch {
                    return;
                }
                editor.reading = false;
                // An edit landed while the read was in flight; the user's text
                // wins over the copy on disk.
                if editor.dirty {
                    return;
                }
                if editor.disk_content == content && editor.writable == writable {
                    return;
                }
                editor.disk_content = content.clone();
                editor.writable = writable;
                editor.dirty = false;
                let pending_goto = editor.pending_goto;
                let state = editor.state.clone();
                state.update(cx, |state, cx| {
                    state.set_read_only(!writable);
                    state.set_content(content, cx);
                    if let Some((line, column)) = pending_goto {
                        let range = state.line_column_range(line, column);
                        state.select_range(range, cx);
                    }
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The editor body: a line-number gutter beside soft-wrapped text.
    ///
    /// The gutter is *painted*, not laid out — one canvas that shapes only the
    /// numbers currently on screen, the way Zed's editor element does. A div per
    /// line would put one layout node per line of the file in every frame, which
    /// is what made large files crawl.
    ///
    /// Row heights come from the text's measured layout rather than a nominal
    /// line height, so a soft-wrapped line still gets exactly one number and the
    /// two columns cannot drift apart down a long file.
    fn render_file_editor_body(
        &mut self,
        relative_path: &str,
        editor_state: &Entity<TextInput>,
        pane_width: f32,
        writable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        const GUTTER_PAD_RIGHT: f32 = 8.0;
        const CONTENT_PAD_TOP: f32 = 6.0;

        let text_size = self.state.code_font_size;
        let line_height = (text_size * 1.5).round();

        // An open find bar follows whichever file this body is showing; a
        // cheap comparison every frame, one recompute on the frame after the
        // visible file actually changes.
        self.sync_file_search_target(relative_path, cx);
        let find_bar = self.render_file_search_bar(pane_width, writable, window, cx);
        if self
            .right_panel_file_editors
            .get(relative_path)
            .is_some_and(|editor| !editor.reading && editor.pending_goto.is_some())
        {
            let target_path = relative_path.to_owned();
            let weak = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let Some((state, location)) = this
                        .right_panel_file_editors
                        .get_mut(&target_path)
                        .filter(|editor| !editor.reading)
                        .and_then(|editor| {
                            Some((editor.state.clone(), editor.pending_goto.take()?))
                        })
                    else {
                        return;
                    };
                    let range = state.read(cx).line_column_range(location.0, location.1);
                    state.update(cx, |state, cx| state.select_range(range.clone(), cx));
                    this.reveal_file_search_match(&state, range.start, cx);
                    cx.notify();
                });
            });
        }

        let theme = Theme::current(cx);
        let field = editor_state.read(cx);
        let line_count = field.content().split('\n').count().max(1);
        let heights = field.wrapped_line_heights();
        // A mono digit advances ~0.6em, so the gutter tracks the font size.
        let digit_width = (text_size * 0.6).ceil();
        let gutter_width = 20.0 + digit_width * (line_count.to_string().len() as f32);
        let content_height = if heights.is_empty() {
            px(line_height) * line_count as f32
        } else {
            heights.iter().fold(Pixels::ZERO, |total, h| total + *h)
        };

        let viewport = self.right_panel_editor_scroll_handle.clone();
        let number_color = theme.text_ghost;
        let gutter = canvas(
            |_, _, _| (),
            move |bounds: gpui::Bounds<Pixels>, _, window: &mut Window, cx: &mut App| {
                let visible = viewport.bounds();
                let mut y = bounds.origin.y;
                for number in 1..=line_count {
                    let height = heights
                        .get(number - 1)
                        .copied()
                        .unwrap_or_else(|| px(line_height));
                    // Everything below the viewport is unreachable from here on.
                    if y > visible.bottom() {
                        break;
                    }
                    if y + height >= visible.top() {
                        let text = SharedString::from(number.to_string());
                        let run = gpui::TextRun {
                            len: text.len(),
                            font: gpui::font(md::render::MONO_FAMILY),
                            color: number_color,
                            ..Default::default()
                        };
                        let line =
                            window
                                .text_system()
                                .shape_line(text, px(text_size), &[run], None);
                        let origin = point(bounds.right() - line.width, y);
                        let _ = line.paint(
                            origin,
                            px(line_height),
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                    y += height;
                }
            },
        )
        .flex_none()
        .w(px(gutter_width - GUTTER_PAD_RIGHT))
        .h(content_height);

        // The find bar sits in normal flow above the scroll region — Zed's
        // buffer-search arrangement — so an open bar pushes the content and
        // its line-number gutter down instead of covering the first lines.
        div()
            .key_context("FileEditorPane")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .font_family(md::render::MONO_FAMILY)
            .text_size(px(text_size))
            .line_height(px(line_height))
            .children(find_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div()
                            .id(SharedString::from(format!("file-editor-{relative_path}")))
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.right_panel_editor_scroll_handle)
                            .child(
                                div()
                                    .w_full()
                                    .pt(px(CONTENT_PAD_TOP))
                                    .pb(px(CONTENT_PAD_TOP))
                                    .flex()
                                    .items_start()
                                    .child(gutter)
                                    .child(div().w(px(GUTTER_PAD_RIGHT)).flex_none())
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .pr(px(10.0))
                                            .child(editor_state.clone()),
                                    ),
                            ),
                    )
                    .child(scrollbar::vertical(
                        &self.right_panel_editor_scroll_handle,
                        &self.right_panel_editor_scrollbar,
                    )),
            )
    }

    /// Flips the global markdown source/preview mode and persists it, so the
    /// choice follows the user across files and sessions.
    fn toggle_markdown_preview(&mut self, cx: &mut Context<Self>) {
        self.state.markdown_preview = !self.state.markdown_preview;
        self.save();
        cx.notify();
    }

    /// The rendered-markdown alternative to the editor body, shown while the
    /// global preview toggle is on. It renders the editor's current text —
    /// unsaved edits included — with the transcript's markdown engine; the
    /// parse is cached per path, so re-rendering an unchanged document costs
    /// `Rc` clones, not a re-parse. Reads only in-memory editor state: the
    /// render path may not touch the filesystem.
    fn render_file_markdown_preview(
        &mut self,
        relative_path: &str,
        editor_state: &Entity<TextInput>,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let palette = MarkdownPalette::from_theme(&theme);
        let mut cache = self.file_preview_markdown.borrow_mut();
        if !matches!(cache.as_ref(), Some((cached, _)) if cached == relative_path) {
            *cache = Some((relative_path.to_owned(), MarkdownView::new()));
        }
        let (_, view) = cache.as_mut().expect("entry ensured above");
        view.set_text(editor_state.read(cx).content(), false);
        let ctx = MarkdownCtx::new(
            format!("file-preview-{relative_path}"),
            &palette,
            MarkdownMetrics::document(self.state.ui_font_size, self.state.code_font_size),
            self.file_preview_selection.clone(),
        )
        .with_math_enabled(self.state.render_math)
        .with_math_context_menu(self.menu_handle("file-preview-math", cx))
        .with_link_handler(self.markdown_link_handler.clone());
        let document = md::render::markdown(view, &ctx);

        let selection_input = {
            let selection = self.file_preview_selection.clone();
            canvas(
                |_, _, _| (),
                move |_, _, window, _| md::render::install_selection_input(window, &selection),
            )
            .absolute()
            .w(px(0.0))
            .h(px(0.0))
        };

        div()
            .flex_1()
            .min_h_0()
            .relative()
            .bg(theme.surface)
            .child(
                div()
                    .id(SharedString::from(format!("file-preview-{relative_path}")))
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.file_preview_scroll_handle)
                    // Painted before the document, so the frame's selection
                    // registry holds exactly this frame's text elements.
                    .child(md::render::frame_reset(self.file_preview_selection.clone()))
                    .child(
                        div()
                            .px(px(16.0))
                            .pt(px(14.0))
                            .pb(px(24.0))
                            .text_color(theme.text)
                            .children(document),
                    ),
            )
            .child(selection_input)
            .child(scrollbar::vertical(
                &self.file_preview_scroll_handle,
                &self.file_preview_scrollbar,
            ))
    }

    /// Picks up an external edit to a file the user has not modified here.
    ///
    /// Reaches the filesystem, so it queues a background read rather than
    /// blocking; the editor keeps showing its current text until that lands.
    fn reload_right_panel_file_if_clean(&mut self, relative_path: &str, cx: &mut Context<Self>) {
        if self
            .right_panel_file_editors
            .get(relative_path)
            .is_none_or(|editor| editor.dirty)
        {
            return;
        }
        self.read_right_panel_file_into_editor(relative_path.to_owned(), cx);
    }

    pub(super) fn reload_clean_right_panel_file_editors(&mut self, cx: &mut Context<Self>) {
        let paths = self
            .right_panel_file_editors
            .iter()
            .filter(|(_, editor)| !editor.dirty)
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in paths {
            self.reload_right_panel_file_if_clean(&path, cx);
        }
    }

    pub(super) fn save_right_panel_file_action(
        &mut self,
        _: &SaveFile,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(relative_path) = self.visible_right_panel_file_path() else {
            return;
        };
        let Some(project_path) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            return;
        };
        let Some(editor) = self.right_panel_file_editors.get(&relative_path) else {
            return;
        };
        if !editor.writable {
            self.show_toast(if editor.reading {
                tr!("files.could_not_save_opening", path = relative_path)
            } else {
                tr!("files.could_not_save_read_only", path = relative_path)
            });
            cx.notify();
            return;
        }

        let content = editor.state.read(cx).content().to_owned();
        let Some(session_id) = self.state.selected_session else {
            return;
        };
        let epoch = if let Some(editor) = self.right_panel_file_editors.get_mut(&relative_path) {
            editor.reading = false;
            editor.read_epoch += 1;
            editor.read_epoch
        } else {
            return;
        };
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let project_path = project_path.clone();
                    let relative_path = relative_path.clone();
                    let content = content.clone();
                    async move {
                        match workspace.request(waku_client::WorkspaceOperation::WriteTextFile {
                            root: project_path,
                            relative_path: PathBuf::from(relative_path),
                            content,
                        })? {
                            waku_client::WorkspaceResult::Ack => Ok(()),
                            _ => anyhow::bail!("the daemon returned an invalid file response"),
                        }
                    }
                })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                if waku.state.selected_session != Some(session_id)
                    || waku
                        .selected_workspace_path()
                        .is_none_or(|path| path != project_path)
                {
                    return;
                }
                match result {
                    Ok(()) => {
                        if let Some(editor) = waku.right_panel_file_editors.get_mut(&relative_path)
                            && editor.read_epoch == epoch
                        {
                            let current = editor.state.read(cx).content();
                            editor.disk_content = content.clone();
                            editor.dirty = current != content;
                        }
                    }
                    Err(error) => waku.show_toast(tr!(
                        "files.could_not_save",
                        path = relative_path,
                        error = error.to_string()
                    )),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn begin_right_panel_file_create(
        &mut self,
        parent_relative_path: String,
        depth: usize,
        is_dir: bool,
        expand_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dir) = expand_directory
            && self.right_panel_expanded_paths.insert(dir)
        {
            self.refresh_right_panel_working_tree(cx);
        }
        self.right_panel_file_rename = Some(RightPanelFileRename::Create {
            parent_relative_path,
            depth,
            is_dir,
        });
        let placeholder = if is_dir {
            tr!("files.new_folder_placeholder")
        } else {
            tr!("files.new_file_placeholder")
        };
        self.right_panel_file_rename_input.update(cx, |input, cx| {
            input.set_placeholder(placeholder, cx);
            input.clear(cx);
        });
        let focus = self.right_panel_file_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    /// Starts the inline rename for a working-tree row. Renaming stays inside
    /// the entry's directory: the field edits the name only.
    pub(super) fn begin_right_panel_file_rename(
        &mut self,
        relative_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .right_panel_working_tree
            .iter()
            .find(|entry| entry.relative_path == relative_path)
            .cloned()
        else {
            return;
        };
        self.right_panel_file_rename = Some(RightPanelFileRename::Rename {
            relative_path,
            absolute_path: entry.absolute_path,
        });
        self.right_panel_file_rename_input.update(cx, |input, cx| {
            input.set_content(entry.name, cx);
            input.select_all_text(cx);
        });
        let focus = self.right_panel_file_rename_input.read(cx).focus();
        window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        cx.notify();
    }

    pub(super) fn commit_right_panel_file_rename(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.right_panel_file_rename.take() else {
            return;
        };
        let new_name = self
            .right_panel_file_rename_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        match edit {
            RightPanelFileRename::Create {
                parent_relative_path,
                is_dir,
                ..
            } => {
                if new_name.is_empty() {
                    cx.notify();
                    return;
                }
                if new_name.contains(['/', '\\', '\0']) || new_name == "." || new_name == ".." {
                    self.show_toast(tr!("files.invalid_name"));
                    cx.notify();
                    return;
                }
                let relative_path = if parent_relative_path.is_empty() {
                    new_name
                } else {
                    let sep = if parent_relative_path.contains('\\') {
                        '\\'
                    } else {
                        '/'
                    };
                    format!("{parent_relative_path}{sep}{new_name}")
                };
                let Some(project_path) = self.selected_workspace_path().map(Path::to_path_buf)
                else {
                    return;
                };
                let Some(session_id) = self.state.selected_session else {
                    return;
                };
                let workspace =
                    waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
                cx.spawn(async move |waku, cx| {
                    let result = cx
                        .background_executor()
                        .spawn({
                            let project_path = project_path.clone();
                            let relative_path = relative_path.clone();
                            async move {
                                let op = if is_dir {
                                    waku_client::WorkspaceOperation::CreateDirectory {
                                        root: project_path,
                                        relative_path: PathBuf::from(relative_path),
                                    }
                                } else {
                                    waku_client::WorkspaceOperation::CreateFile {
                                        root: project_path,
                                        relative_path: PathBuf::from(relative_path),
                                    }
                                };
                                match workspace.request(op)? {
                                    waku_client::WorkspaceResult::Ack => Ok(()),
                                    _ => anyhow::bail!("the daemon returned an invalid response"),
                                }
                            }
                        })
                        .await;
                    let _ = waku.update(cx, |waku, cx| {
                        if waku.state.selected_session != Some(session_id)
                            || waku
                                .selected_workspace_path()
                                .is_none_or(|path| path != project_path)
                        {
                            return;
                        }
                        match result {
                            Ok(()) => {
                                if is_dir {
                                    waku.right_panel_expanded_paths
                                        .insert(project_path.join(&relative_path));
                                }
                                waku.refresh_right_panel_working_tree(cx);
                                waku.invalidate_composer_sources(cx);
                                if !is_dir {
                                    waku.open_right_panel_file(relative_path, cx);
                                }
                                cx.notify();
                            }
                            Err(error) => {
                                waku.show_toast(tr!(
                                    "files.create_failed",
                                    path = relative_path,
                                    error = error.to_string()
                                ));
                                cx.notify();
                            }
                        }
                    });
                })
                .detach();
            }
            RightPanelFileRename::Rename { relative_path, .. } => {
                let old_name = relative_path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                if new_name.is_empty() || new_name == old_name {
                    cx.notify();
                    return;
                }
                if new_name.contains(['/', '\\', '\0']) || new_name == "." || new_name == ".." {
                    self.show_toast(tr!("files.invalid_name"));
                    cx.notify();
                    return;
                }
                // Preserve whatever separator convention the tree itself carries.
                let to_relative = match relative_path.rfind(['/', '\\']) {
                    Some(index) => format!("{}{}", &relative_path[..=index], new_name),
                    None => new_name.clone(),
                };

                let Some(project_path) = self.selected_workspace_path().map(Path::to_path_buf)
                else {
                    return;
                };
                let Some(session_id) = self.state.selected_session else {
                    return;
                };
                let workspace =
                    waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
                let from_relative = relative_path;
                cx.spawn(async move |waku, cx| {
                    let result = cx
                        .background_executor()
                        .spawn({
                            let project_path = project_path.clone();
                            let from_relative = from_relative.clone();
                            let to_relative = to_relative.clone();
                            async move {
                                match workspace.request(
                                    waku_client::WorkspaceOperation::RenamePath {
                                        root: project_path,
                                        from_relative_path: PathBuf::from(from_relative),
                                        to_relative_path: PathBuf::from(to_relative),
                                    },
                                )? {
                                    waku_client::WorkspaceResult::Ack => Ok(()),
                                    _ => anyhow::bail!(
                                        "the daemon returned an invalid rename response"
                                    ),
                                }
                            }
                        })
                        .await;
                    let _ = waku.update(cx, |waku, cx| {
                        if waku.state.selected_session != Some(session_id)
                            || waku
                                .selected_workspace_path()
                                .is_none_or(|path| path != project_path)
                        {
                            return;
                        }
                        match result {
                            Ok(()) => {
                                waku.retarget_right_panel_file_paths(&from_relative, &to_relative);
                                waku.refresh_right_panel_working_tree(cx);
                                cx.notify();
                            }
                            Err(error) => {
                                waku.show_toast(tr!(
                                    "files.rename_failed",
                                    path = from_relative,
                                    error = error.to_string()
                                ));
                                cx.notify();
                            }
                        }
                    });
                })
                .detach();
            }
        }
    }

    pub(super) fn cancel_right_panel_file_rename(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.right_panel_file_rename.take().is_none() {
            return;
        }
        let focus = self.composer_focus(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Moves every reference to a just-renamed entry — open editors, tabs, the
    /// selected file, and the expanded-directory set — onto its new path.
    fn retarget_right_panel_file_paths(&mut self, from_relative: &str, to_relative: &str) {
        fn retarget(key: &str, from: &str, to: &str) -> Option<String> {
            if key == from {
                return Some(to.to_owned());
            }
            for separator in ['/', '\\'] {
                if let Some(rest) = key.strip_prefix(&format!("{from}{separator}")) {
                    return Some(format!("{to}{separator}{rest}"));
                }
            }
            None
        }
        if let Some(editor) = self.right_panel_file_editors.remove(from_relative) {
            self.right_panel_file_editors
                .insert(to_relative.to_owned(), editor);
        } else {
            let editor_moves: Vec<(String, String)> = self
                .right_panel_file_editors
                .keys()
                .filter_map(|key| {
                    retarget(key, from_relative, to_relative).map(|to| (key.clone(), to))
                })
                .collect();
            for (from, to) in editor_moves {
                if let Some(editor) = self.right_panel_file_editors.remove(&from) {
                    self.right_panel_file_editors.insert(to, editor);
                }
            }
        }
        for surface in &mut self.right_panel_surfaces {
            if let RightPanelSurface::File(path) = surface
                && let Some(retargeted) = retarget(path, from_relative, to_relative)
            {
                *path = retargeted;
            }
        }
        if let Some(selected) = self.right_panel_files_selected_path.as_mut()
            && let Some(retargeted) = retarget(selected, from_relative, to_relative)
        {
            *selected = retargeted;
        }
        if let Some(RightPanelFileRename::Rename {
            relative_path,
            absolute_path,
        }) = self.right_panel_file_rename.as_ref()
            && let Some(retargeted) = retarget(relative_path, from_relative, to_relative)
        {
            self.right_panel_file_rename = Some(RightPanelFileRename::Rename {
                relative_path: retargeted,
                absolute_path: absolute_path.clone(),
            });
        }
        let absolute_moves: Vec<(PathBuf, PathBuf)> = self
            .right_panel_expanded_paths
            .iter()
            .filter_map(|path| {
                let key = path.to_string_lossy();
                retarget(&key, from_relative, to_relative)
                    .map(|to| (path.clone(), PathBuf::from(to)))
            })
            .collect();
        for (from, to) in absolute_moves {
            self.right_panel_expanded_paths.remove(&from);
            self.right_panel_expanded_paths.insert(to);
        }
    }

    /// Streams a remote (or local) file to a path of the user's choosing, one
    /// chunk per request, so the transfer is bounded only by disk space — the
    /// VS Code remote-download shape. The bytes travel over the daemon RPC,
    /// so this works unchanged over SSH and WSL bridges.
    pub(super) fn download_right_panel_file(
        &mut self,
        relative_path: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(project_path) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return;
        };
        let default_directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_default();
        let receiver = cx.prompt_for_new_path(&default_directory, Some(&name));
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        let display_name = name.clone();
        cx.spawn(async move |waku, cx| {
            let Ok(Ok(Some(destination))) = receiver.await else {
                return;
            };
            let Ok((download_id, cancel)) =
                waku.update(cx, |waku, cx| waku.begin_download(display_name.clone(), cx))
            else {
                return;
            };
            enum Outcome {
                Complete,
                Canceled,
                Failed(anyhow::Error),
            }
            let mut offset: u64 = 0;
            let outcome = loop {
                if cancel.load(Ordering::Acquire) {
                    break Outcome::Canceled;
                }
                // One request plus one bounded write, both off the UI thread.
                let step = cx
                    .background_executor()
                    .spawn({
                        let workspace = workspace.clone();
                        let project_path = project_path.clone();
                        let relative_path = relative_path.clone();
                        let destination = destination.clone();
                        async move {
                            let response = workspace.request(
                                waku_client::WorkspaceOperation::ReadFileChunk {
                                    root: project_path,
                                    relative_path: PathBuf::from(relative_path),
                                    offset,
                                    length: DOWNLOAD_CHUNK_BYTES,
                                },
                            )?;
                            let waku_client::WorkspaceResult::FileChunk {
                                bytes,
                                total_bytes,
                            } = response
                            else {
                                anyhow::bail!("the daemon returned an invalid file response");
                            };
                            let bytes =
                                base64::engine::general_purpose::STANDARD.decode(bytes.trim())?;
                            let mut file = std::fs::OpenOptions::new();
                            let mut file = if offset == 0 {
                                file.create(true).write(true).truncate(true)
                            } else {
                                file.append(true)
                            }
                            .open(&destination)?;
                            use std::io::Write as _;
                            file.write_all(&bytes)?;
                            Ok((bytes.len() as u64, total_bytes))
                        }
                    })
                    .await;
                match step {
                    Ok((chunk_bytes, total_bytes)) => {
                        offset += chunk_bytes;
                        if waku
                            .update(cx, |waku, cx| {
                                waku.update_download_progress(
                                    download_id,
                                    offset,
                                    total_bytes,
                                    cx,
                                )
                            })
                            .is_err()
                        {
                            // The window went away; stop rather than stream
                            // into a card nobody will see.
                            break Outcome::Canceled;
                        }
                        if chunk_bytes == 0 {
                            if offset != total_bytes {
                                break Outcome::Failed(anyhow::anyhow!(
                                    "the file changed size from {total_bytes} to {offset} bytes while downloading"
                                ));
                            }
                            break Outcome::Complete;
                        }
                    }
                    Err(error) => break Outcome::Failed(error),
                }
            };
            let cleanup = cx.background_executor().spawn({
                let destination = destination.clone();
                let keep_file = matches!(outcome, Outcome::Complete);
                async move {
                    if !keep_file {
                        // A canceled or failed transfer leaves nothing behind.
                        let _ = std::fs::remove_file(&destination);
                    }
                }
            });
            cleanup.await;
            let _ = waku.update(cx, |waku, cx| {
                waku.end_download(download_id, cx);
                match outcome {
                    Outcome::Complete => waku.show_success_toast(tr!(
                        "files.download_saved",
                        name = display_name,
                        path = destination.display()
                    )),
                    Outcome::Canceled => {
                        waku.show_toast(tr!("files.download_canceled", name = display_name))
                    }
                    Outcome::Failed(error) => waku.show_toast(tr!(
                        "files.download_failed",
                        error = error.to_string()
                    )),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Parks a delete behind the confirm dialog. Remote deletion is
    /// unrecoverable, so it never runs straight from the menu.
    pub(super) fn request_right_panel_file_delete(
        &mut self,
        relative_path: String,
        name: String,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cancel_focus = cx.focus_handle();
        let confirm_focus = cx.focus_handle();
        self.right_panel_delete_confirm = Some(RightPanelDeleteConfirm {
            relative_path,
            name,
            is_dir,
            cancel_focus: cancel_focus.clone(),
            confirm_focus,
        });
        // Like the other deferred layers, the dialog joins the dispatch tree
        // only after it has drawn; focus the safe choice two frames later so
        // keys cannot fall through to the transcript beneath it.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&cancel_focus, cx));
        });
        cx.notify();
    }

    pub(super) fn confirm_right_panel_file_delete_action(
        &mut self,
        _: &ConfirmRightPanelFileDelete,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(confirm) = self.right_panel_delete_confirm.take() else {
            return;
        };
        let Some(project_path) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return;
        };
        let Some(session_id) = self.state.selected_session else {
            return;
        };
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let project_path = project_path.clone();
                    let relative_path = confirm.relative_path.clone();
                    async move {
                        match workspace.request(waku_client::WorkspaceOperation::DeletePath {
                            root: project_path,
                            relative_path: PathBuf::from(relative_path),
                        })? {
                            waku_client::WorkspaceResult::Ack => Ok(()),
                            _ => anyhow::bail!("the daemon returned an invalid delete response"),
                        }
                    }
                })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                if waku.state.selected_session != Some(session_id)
                    || waku
                        .selected_workspace_path()
                        .is_none_or(|path| path != project_path)
                {
                    return;
                }
                match result {
                    Ok(()) => {
                        waku.close_right_panel_paths_under(
                            &confirm.relative_path,
                            confirm.is_dir,
                            cx,
                        );
                        if let Some(absolute) = waku
                            .right_panel_working_tree
                            .iter()
                            .find(|entry| entry.relative_path == confirm.relative_path)
                            .map(|entry| entry.absolute_path.clone())
                        {
                            waku.right_panel_expanded_paths
                                .retain(|path| path != &absolute && !path.starts_with(&absolute));
                        }
                        waku.refresh_right_panel_working_tree(cx);
                        cx.notify();
                    }
                    Err(error) => {
                        waku.show_toast(tr!(
                            "files.delete_failed",
                            path = confirm.relative_path,
                            error = error.to_string()
                        ));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn dismiss_right_panel_file_delete_action(
        &mut self,
        _: &DismissRightPanelFileDelete,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.right_panel_delete_confirm = None;
        cx.notify();
    }

    /// Closes every open editor and tab that a deleted path leaves dangling:
    /// the entry itself, and — for a directory — everything under it.
    fn close_right_panel_paths_under(
        &mut self,
        relative_path: &str,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        fn path_matches(key: &str, relative_path: &str, is_dir: bool) -> bool {
            if key == relative_path {
                return true;
            }
            is_dir
                && ['/', '\\']
                    .iter()
                    .any(|separator| key.starts_with(&format!("{relative_path}{separator}")))
        }
        loop {
            let index = self
                .right_panel_surfaces
                .iter()
                .position(|surface| match surface {
                    RightPanelSurface::File(path) => path_matches(path, relative_path, is_dir),
                    _ => false,
                });
            match index {
                Some(index) => self.close_right_panel_surface(index, cx),
                None => break,
            }
        }
        self.right_panel_file_editors
            .retain(|key, _| !path_matches(key, relative_path, is_dir));
        if self
            .right_panel_files_selected_path
            .as_deref()
            .is_some_and(|path| path_matches(path, relative_path, is_dir))
        {
            self.right_panel_files_selected_path = None;
        }
    }

    fn render_right_panel_diff(
        &mut self,
        panel_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let toolbar = self.render_right_panel_diff_toolbar(cx);
        let content = match self.right_panel_diff_snapshot.clone() {
            Some(snapshot) => {
                let tree_width = fitted_file_tree_width(
                    panel_width,
                    self.right_panel_file_tree_width.max(220.0),
                );
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .child(self.render_right_panel_unified_diff(snapshot.clone(), cx))
                    .child(
                        div()
                            .w(px(tree_width))
                            .min_w(px(FILE_TREE_MIN_WIDTH))
                            .h_full()
                            .flex_none()
                            .relative()
                            .border_l_1()
                            .border_color(theme.border_strong)
                            .child(self.render_right_panel_diff_tree(window, cx))
                            .child(self.render_panel_resize_handle(
                                "right-panel-diff-tree-resize-handle",
                                PanelResizeTarget::FileTree,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
            None if self.right_panel_diff_loading => self
                .render_right_panel_empty_message(
                    tr!("diff.loading"),
                    tr!("diff.loading_description"),
                    cx,
                )
                .into_any_element(),
            None if self.right_panel_diff_error.is_some() => self
                .render_right_panel_empty_message(
                    tr!("diff.unavailable"),
                    self.right_panel_diff_error.clone().unwrap_or_default(),
                    cx,
                )
                .into_any_element(),
            None => self
                .render_right_panel_empty_message(
                    tr!("diff.no_changes"),
                    tr!("diff.no_changes_description"),
                    cx,
                )
                .into_any_element(),
        };

        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .relative()
            .flex()
            .flex_col()
            .child(md::render::frame_reset(
                self.right_panel_diff_selection.clone(),
            ))
            .child(toolbar)
            .child(content)
            .child(self.right_panel_diff_selection_input())
    }

    fn render_right_panel_diff_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let selected = self.right_panel_diff_source;
        let latest_turn = self.latest_review_turn_source();
        let source_label = self.review_diff_source_label(selected);
        let weak = cx.entity().downgrade();
        let handle = self.menu_handle("right-panel-diff-source", cx);
        let source = dropdown_menu(
            MenuChip::new("right-panel-diff-source")
                .label(source_label)
                .height(px(28.0))
                .background(theme.surface)
                .selected(handle.is_open()),
            "right-panel-diff-source-menu",
            &handle,
            MenuAlign::BelowLeft,
            move |_| {
                let mut items = Vec::new();
                let last_turn_source = latest_turn.unwrap_or_default();
                let last_turn_weak = weak.clone();
                items.push(
                    MenuItem::new(tr!("diff.source_last_turn"), move |_, cx| {
                        let _ = last_turn_weak.update(cx, |this, cx| {
                            this.set_right_panel_diff_source(last_turn_source, cx)
                        });
                    })
                    .selected(latest_turn == Some(selected))
                    .disabled(latest_turn.is_none()),
                );
                items.push(MenuItem::Separator);
                for (choice, label) in [
                    (
                        ReviewDiffSource::Uncommitted,
                        tr!("diff.source_uncommitted"),
                    ),
                    (ReviewDiffSource::Unstaged, tr!("diff.source_unstaged")),
                    (ReviewDiffSource::Staged, tr!("diff.source_staged")),
                ] {
                    let choice_weak = weak.clone();
                    items.push(
                        MenuItem::new(label, move |_, cx| {
                            let _ = choice_weak.update(cx, |this, cx| {
                                this.set_right_panel_diff_source(choice, cx)
                            });
                        })
                        .selected(choice == selected),
                    );
                }
                items.push(MenuItem::Separator);
                for (choice, label) in [
                    (ReviewDiffSource::Committed, tr!("diff.source_committed")),
                    (ReviewDiffSource::Branch, tr!("diff.source_branch")),
                ] {
                    let choice_weak = weak.clone();
                    items.push(
                        MenuItem::new(label, move |_, cx| {
                            let _ = choice_weak.update(cx, |this, cx| {
                                this.set_right_panel_diff_source(choice, cx)
                            });
                        })
                        .selected(choice == selected),
                    );
                }
                items
            },
        );

        let (additions, deletions, truncated) = self
            .right_panel_diff_snapshot
            .as_ref()
            .map_or((0, 0, false), |snapshot| {
                (snapshot.additions, snapshot.deletions, snapshot.truncated)
            });
        let refresh_focus = self.transcript_control_focus("right-panel-diff-refresh", cx);
        let refresh_icon: AnyElement = if self.right_panel_diff_loading {
            motion::spin(icon("icons/loader-circle.svg", 12.0, theme.text_tertiary))
        } else {
            icon("icons/rotate-cw.svg", 12.0, theme.text_tertiary).into_any_element()
        };
        let refresh = div()
            .id("right-panel-diff-refresh")
            .track_focus(&refresh_focus)
            .tab_index(0)
            .size(px(28.0))
            .rounded(px(7.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay))
            .child(refresh_icon)
            .tooltip(|window, cx| Tooltip::new(tr!("diff.refresh")).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.refresh_right_panel_diff(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.refresh_right_panel_diff(cx);
                    cx.stop_propagation();
                }
            }));

        div()
            .h(px(44.0))
            .flex_none()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .child(source)
            .child(
                div()
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.success)
                    .child(format!("+{additions}")),
            )
            .child(
                div()
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.danger)
                    .child(format!("-{deletions}")),
            )
            .when(truncated, |row| {
                row.child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(theme.warning)
                        .child(tr!("diff.truncated")),
                )
            })
            .child(div().flex_1())
            .child(refresh)
            .into_any_element()
    }

    fn render_right_panel_unified_diff(
        &self,
        snapshot: Arc<ReviewDiffSnapshot>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if snapshot.files.is_empty() {
            return self
                .render_right_panel_empty_message(
                    tr!("diff.no_changes"),
                    tr!("diff.no_changes_description"),
                    cx,
                )
                .into_any_element();
        }
        let sticky_header = self.render_right_panel_diff_sticky_header(&snapshot, cx);
        let entity = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .relative()
            .overflow_hidden()
            .child(
                list(
                    self.right_panel_diff_list_state.clone(),
                    move |index, _window, cx| {
                        entity
                            .upgrade()
                            .map(|entity| {
                                entity.update(cx, |this, cx| {
                                    this.render_right_panel_diff_line(index, cx)
                                })
                            })
                            .unwrap_or_else(|| div().into_any_element())
                    },
                )
                .size_full(),
            )
            .when_some(sticky_header, |container, header| container.child(header))
            .child(scrollbar::vertical(
                &self.right_panel_diff_list_state,
                &self.right_panel_diff_scrollbar,
            ))
            .into_any_element()
    }

    fn render_right_panel_diff_sticky_header(
        &self,
        snapshot: &ReviewDiffSnapshot,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let scroll_top = self.right_panel_diff_list_state.logical_scroll_top();
        let (header_index, next_header_index) = snapshot.file_headers_around(scroll_top.item_ix)?;
        let needs_sticky = header_index < scroll_top.item_ix
            || (header_index == scroll_top.item_ix && scroll_top.offset_in_item > px(0.));
        if !needs_sticky {
            return None;
        }

        let line = snapshot.lines.get(header_index)?;
        let file = snapshot.files.get(line.file_index)?;
        let top_offset = next_header_index
            .and_then(|next_header_index| {
                let bounds = self
                    .right_panel_diff_list_state
                    .bounds_for_item(next_header_index)?;
                let viewport = self.right_panel_diff_list_state.viewport_bounds();
                let y_in_viewport = bounds.origin.y - viewport.origin.y;
                (y_in_viewport < bounds.size.height).then_some(y_in_viewport - bounds.size.height)
            })
            .unwrap_or(px(0.));

        Some(
            div()
                .absolute()
                .top(top_offset)
                .left_0()
                .w_full()
                .child(self.render_right_panel_diff_file_header(header_index, file, true, cx))
                .into_any_element(),
        )
    }

    fn render_right_panel_diff_file_header(
        &self,
        index: usize,
        file: &crate::review_diff::File,
        sticky: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let id_prefix = if sticky {
            "review-diff-sticky-file"
        } else {
            "review-diff-file"
        };
        div()
            .id(SharedString::from(format!("{id_prefix}-{index}")))
            .w_full()
            .min_w_0()
            .h(px(REVIEW_DIFF_FILE_HEADER_HEIGHT))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .when(sticky, |header| header.block_mouse_except_scroll())
            .child(file_icon(file_icon_for_path(&file.path), 14.0))
            .child(
                div()
                    .id(SharedString::from(format!("{id_prefix}-path-{index}")))
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary)
                    .tooltip(Tooltip::text(file.path.clone()))
                    .child(file.path.clone()),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.success)
                    .child(format!("+{}", file.additions)),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.danger)
                    .child(format!("-{}", file.deletions)),
            )
            .into_any_element()
    }

    fn render_right_panel_diff_line(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.right_panel_diff_snapshot.as_ref() else {
            return div().into_any_element();
        };
        let Some(line) = snapshot.lines.get(index) else {
            return div().into_any_element();
        };
        let Some(file) = snapshot.files.get(line.file_index) else {
            return div().into_any_element();
        };
        let theme = Theme::current(cx);
        let style = DiffRowStyle::review(self.state.code_font_size);
        // Chrome rows keep their gutters flush with the code rows'.
        let gutter_width = style.gutter_width();

        match &line.kind {
            crate::review_diff::LineKind::FileHeader => {
                self.render_right_panel_diff_file_header(index, file, false, cx)
            }
            crate::review_diff::LineKind::Gap(gap) => {
                let expandable = gap.is_expandable();
                let chunked = gap.count() > crate::review_diff::DEFAULT_EXPANSION_LINE_COUNT as u32;
                let directions = review_diff_gap_directions(gap.position, chunked);
                let two_directions = directions.len() > 1;
                let gutter = div()
                    .w(px(gutter_width))
                    .h_full()
                    .flex_none()
                    .flex()
                    .when(two_directions, |gutter| gutter.flex_col())
                    .border_r_1()
                    .border_color(theme.border)
                    .bg(theme.overlay)
                    .when(expandable, |mut gutter| {
                        for (button_index, direction) in directions.iter().copied().enumerate() {
                            gutter = gutter.child(self.render_right_panel_diff_gap_action(
                                index,
                                gap.id,
                                direction,
                                review_diff_gap_icon_path(direction),
                                review_diff_gap_tooltip(direction),
                                two_directions,
                                two_directions && button_index == 0,
                                cx,
                            ));
                        }
                        gutter
                    });
                let label_focus = self
                    .transcript_control_focus(format!("right-panel-diff-gap-{}-label", gap.id), cx);
                let label = div()
                    .id(SharedString::from(format!(
                        "right-panel-diff-gap-{}-label",
                        gap.id
                    )))
                    .track_focus(&label_focus)
                    .h_full()
                    .min_w_0()
                    .flex_1()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .bg(theme.overlay)
                    .child(tr!("diff.unmodified_lines", count = gap.count()))
                    .when(expandable, |label| {
                        label
                            .tab_index(0)
                            .cursor_default()
                            .focus_visible(|style| style.border_1().border_color(theme.accent))
                            .hover(|style| {
                                style
                                    .bg(theme.overlay_strong)
                                    .text_color(theme.text_secondary)
                            })
                            .active(|style| style.bg(theme.overlay))
                            .tooltip(Tooltip::text(tr!("diff.expand_context")))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                                let direction = if event.modifiers().shift {
                                    crate::review_diff::ExpansionDirection::All
                                } else {
                                    crate::review_diff::ExpansionDirection::Both
                                };
                                this.expand_right_panel_diff_gap(index, direction, cx);
                                cx.stop_propagation();
                            }))
                            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    let direction = if event.keystroke.modifiers.shift {
                                        crate::review_diff::ExpansionDirection::All
                                    } else {
                                        crate::review_diff::ExpansionDirection::Both
                                    };
                                    this.expand_right_panel_diff_gap(index, direction, cx);
                                    cx.stop_propagation();
                                }
                            }))
                    });
                div()
                    .h(px(32.0))
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .text_size(px(12.5))
                    .text_color(theme.text_tertiary)
                    .child(gutter)
                    .child(label)
                    .into_any_element()
            }
            crate::review_diff::LineKind::HunkHeader => div()
                .min_h(px(24.0))
                .w_full()
                .min_w_0()
                .flex()
                .items_stretch()
                .font_family(md::render::MONO_FAMILY)
                .text_size(px(12.5))
                .line_height(px(16.0))
                .text_color(theme.text_tertiary)
                .child(
                    div()
                        .w(px(gutter_width))
                        .min_h(px(24.0))
                        .self_stretch()
                        .flex_none()
                        .border_r_1()
                        .border_color(theme.border)
                        .bg(theme.overlay),
                )
                .child(
                    div()
                        .min_h(px(24.0))
                        .min_w_0()
                        .flex_1()
                        .px(px(12.0))
                        .py(px(4.0))
                        .flex()
                        .items_start()
                        .overflow_hidden()
                        .whitespace_normal()
                        .bg(theme.overlay)
                        .child(line.content.clone()),
                )
                .into_any_element(),
            crate::review_diff::LineKind::Meta => div()
                .min_h(px(24.0))
                .w_full()
                .min_w_0()
                .flex()
                .items_stretch()
                .font_family(md::render::MONO_FAMILY)
                .text_size(px(12.5))
                .line_height(px(16.0))
                .text_color(theme.text_tertiary)
                .child(
                    div()
                        .w(px(gutter_width))
                        .min_h(px(24.0))
                        .self_stretch()
                        .flex_none(),
                )
                .child(
                    div()
                        .min_h(px(24.0))
                        .min_w_0()
                        .flex_1()
                        .py(px(4.0))
                        .overflow_hidden()
                        .whitespace_normal()
                        .pr(px(10.0))
                        .child(line.content.clone()),
                )
                .into_any_element(),
            crate::review_diff::LineKind::Context
            | crate::review_diff::LineKind::Addition
            | crate::review_diff::LineKind::Deletion => render_diff_code_row(
                line,
                index,
                "review-diff",
                &self.right_panel_diff_selection,
                style,
                &theme,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_right_panel_diff_gap_action(
        &self,
        line_index: usize,
        gap_id: u64,
        direction: crate::review_diff::ExpansionDirection,
        icon_path: &'static str,
        tooltip: String,
        compact_half: bool,
        border_bottom: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = Theme::current(cx);
        let direction_name = match direction {
            crate::review_diff::ExpansionDirection::Start => "start",
            crate::review_diff::ExpansionDirection::End => "end",
            crate::review_diff::ExpansionDirection::Both => "both",
            crate::review_diff::ExpansionDirection::All => "all",
        };
        let focus = self.transcript_control_focus(
            format!("right-panel-diff-gap-{gap_id}-button-{direction_name}"),
            cx,
        );
        div()
            .id(SharedString::from(format!(
                "right-panel-diff-gap-{gap_id}-button-{direction_name}"
            )))
            .track_focus(&focus)
            .tab_index(0)
            .w_full()
            .h_full()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .cursor_default()
            .when(compact_half, |button| button.h(px(16.0)).flex_none())
            .when(border_bottom, |button| {
                button.border_b_1().border_color(theme.border)
            })
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay_strong))
            .active(|style| style.bg(theme.overlay))
            .tooltip(Tooltip::text(tooltip))
            .child(icon(icon_path, 11.0, theme.text_tertiary))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                let direction = if event.modifiers().shift {
                    crate::review_diff::ExpansionDirection::All
                } else {
                    direction
                };
                this.expand_right_panel_diff_gap(line_index, direction, cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    let direction = if event.keystroke.modifiers.shift {
                        crate::review_diff::ExpansionDirection::All
                    } else {
                        direction
                    };
                    this.expand_right_panel_diff_gap(line_index, direction, cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn expand_right_panel_diff_gap(
        &mut self,
        line_index: usize,
        direction: crate::review_diff::ExpansionDirection,
        cx: &mut Context<Self>,
    ) {
        let expansion = self
            .right_panel_diff_snapshot
            .as_mut()
            .and_then(|snapshot| Arc::make_mut(snapshot).expand_gap(line_index, direction));
        let Some(expansion) = expansion else {
            return;
        };
        self.right_panel_diff_list_state
            .splice(line_index..line_index + 1, expansion.replacement_count);
        cx.notify();
    }

    /// One listener set covers every selectable code line registered while
    /// the virtualized Review list paints this frame.
    fn right_panel_diff_selection_input(&self) -> impl IntoElement {
        let selection = self.right_panel_diff_selection.clone();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| md::render::install_selection_input(window, &selection),
        )
        .absolute()
        .w(px(0.0))
        .h(px(0.0))
    }

    fn render_right_panel_diff_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let focus = self.transcript_control_focus("right-panel-diff-tree", cx);
        let tree_focused = focus.is_focused(window);
        let entity = cx.entity().downgrade();
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(44.0))
                    .flex_none()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        TextField::new(
                            "right-panel-diff-filter",
                            self.right_panel_diff_filter.clone(),
                        )
                        .icon("icons/search.svg", 13.0)
                        .w_full(),
                    ),
            )
            .child(
                div()
                    .id("right-panel-diff-tree")
                    .track_focus(&focus)
                    .tab_index(0)
                    .key_context("ReviewDiffTree")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        this.right_panel_diff_tree_key_down(event, window, cx)
                    }))
                    .child(
                        list(
                            self.right_panel_diff_tree_list_state.clone(),
                            move |index, _window, cx| {
                                entity
                                    .upgrade()
                                    .map(|entity| {
                                        entity.update(cx, |this, cx| {
                                            this.render_right_panel_diff_tree_row(
                                                index,
                                                tree_focused,
                                                cx,
                                            )
                                        })
                                    })
                                    .unwrap_or_else(|| div().into_any_element())
                            },
                        )
                        .size_full()
                        .py(px(4.0)),
                    )
                    .child(scrollbar::vertical(
                        &self.right_panel_diff_tree_list_state,
                        &self.right_panel_diff_tree_scrollbar,
                    )),
            )
    }

    fn render_right_panel_diff_tree_row(
        &self,
        index: usize,
        tree_focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.right_panel_diff_tree_rows.borrow().get(index).cloned() else {
            return div().h(px(30.0)).into_any_element();
        };
        let theme = Theme::current(cx);
        let cursor = tree_focused && self.right_panel_diff_tree_cursor == Some(index);
        match row {
            ReviewDiffTreeRow::Directory {
                path,
                name,
                depth,
                expanded,
            } => div()
                .w_full()
                .h(px(30.0))
                .px(px(6.0))
                .flex()
                .items_center()
                .child(
                    div()
                        .id(SharedString::from(format!("review-diff-directory-{path}")))
                        .h(px(26.0))
                        .flex_1()
                        .min_w_0()
                        .pl(px(7.0 + depth as f32 * 14.0))
                        .pr(px(7.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .cursor_default()
                        .when(cursor, |row| row.bg(theme.overlay_strong))
                        .when(!cursor, |row| row.hover(|row| row.bg(theme.overlay)))
                        .child(icon(
                            if expanded {
                                "icons/chevron-down.svg"
                            } else {
                                "icons/chevron-right.svg"
                            },
                            10.0,
                            theme.text_ghost,
                        ))
                        .child(icon("icons/folder.svg", 13.0, theme.text_tertiary))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(sp(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_secondary)
                                .child(name),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let focus = this.transcript_control_focus("right-panel-diff-tree", cx);
                            focus.focus(window, cx);
                            this.right_panel_diff_tree_cursor = Some(index);
                            this.toggle_right_panel_diff_directory(path.clone(), cx);
                        })),
                )
                .into_any_element(),
            ReviewDiffTreeRow::File { file_index, depth } => {
                let Some(snapshot) = self.right_panel_diff_snapshot.as_ref() else {
                    return div().h(px(30.0)).into_any_element();
                };
                let Some(file) = snapshot.files.get(file_index) else {
                    return div().h(px(30.0)).into_any_element();
                };
                let path = file.path.clone();
                let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
                let selected = self.right_panel_diff_selected_file == Some(file_index);
                let (status, status_color) = match file.status {
                    crate::review_diff::FileStatus::Added => ("A", theme.success),
                    crate::review_diff::FileStatus::Deleted => ("D", theme.danger),
                    crate::review_diff::FileStatus::Binary => ("B", theme.warning),
                    crate::review_diff::FileStatus::Modified => ("M", theme.warning),
                };
                div()
                    .w_full()
                    .h(px(30.0))
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .id(SharedString::from(format!("review-diff-tree-file-{path}")))
                            .h(px(26.0))
                            .flex_1()
                            .min_w_0()
                            .pl(px(23.0 + depth as f32 * 14.0))
                            .pr(px(7.0))
                            .rounded(px(5.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .cursor_default()
                            .when(selected && cursor, |row| row.bg(theme.overlay_strong))
                            .when(selected ^ cursor, |row| row.bg(theme.overlay))
                            .when(!selected && !cursor, |row| {
                                row.hover(|row| row.bg(theme.overlay))
                            })
                            .child(file_icon(file_icon_for_path(&path), 13.0))
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "review-diff-tree-file-path-{file_index}"
                                    )))
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_size(sp(12.5))
                                    .text_color(if selected {
                                        theme.text
                                    } else {
                                        theme.text_secondary
                                    })
                                    .tooltip(Tooltip::text(path.clone()))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .w(px(18.0))
                                    .h(px(18.0))
                                    .flex_none()
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(status_color.opacity(0.65))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(sp(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(status_color)
                                    .child(status),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let focus =
                                    this.transcript_control_focus("right-panel-diff-tree", cx);
                                focus.focus(window, cx);
                                this.right_panel_diff_tree_cursor = Some(index);
                                this.select_right_panel_diff_file(file_index, cx);
                            })),
                    )
                    .into_any_element()
            }
        }
    }

    pub(super) fn render_right_panel_empty_message(
        &self,
        title: String,
        description: String,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .pb(px(32.0))
            .child(
                div()
                    .text_size(sp(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .max_w(px(300.0))
                    .text_center()
                    .text_size(sp(12.5))
                    .line_height(sp(17.0))
                    .text_color(theme.text_tertiary)
                    .child(description),
            )
    }

    /// Re-reads whichever workspace surface is on screen.
    pub(super) fn refresh_workspace_surfaces(&mut self, cx: &mut Context<Self>) {
        match self.active_right_panel_surface() {
            Some(RightPanelSurface::Diff) => self.refresh_right_panel_diff(cx),
            Some(RightPanelSurface::GitGraph) => {
                if let Some(scope) = self.selected_panel_scope() {
                    self.git_graph_snapshots.invalidate(&scope);
                }
                cx.notify();
            }
            Some(RightPanelSurface::Files | RightPanelSurface::File(_)) => {
                self.refresh_right_panel_working_tree(cx)
            }
            _ => {}
        }
    }

    /// Re-walks the project's working tree.
    ///
    /// `read_dir` plus a `stat` per entry, recursively over expanded
    /// directories — filesystem I/O, so it runs on the background executor and
    /// the panel keeps drawing the previous listing until the result lands.
    /// Called when the tree's inputs change, never from a frame.
    fn refresh_right_panel_working_tree(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.selected_panel_scope() else {
            self.right_panel_working_tree.clear();
            return;
        };
        // The tree on disk moves under us, and the expanded set may just have
        // changed, so a cached listing is only good until something asks again.
        self.working_trees.invalidate(&scope);
        match self.working_trees.read(&scope) {
            Query::Ready(entries) => self.right_panel_working_tree = (*entries).clone(),
            Query::Pending => {}
            Query::Missing(token) => {
                let expanded = self.right_panel_expanded_paths.clone();
                let workspace =
                    waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
                cx.spawn(async move |waku, cx| {
                    let entries = cx
                        .background_executor()
                        .spawn({
                            let path = scope.workspace.clone();
                            async move {
                                match workspace.request(waku_client::WorkspaceOperation::ListTree {
                                    root: path,
                                    expanded_paths: expanded.into_iter().collect(),
                                }) {
                                    Ok(waku_client::WorkspaceResult::WorkingTree { entries }) => {
                                        entries
                                            .into_iter()
                                            .map(|entry| WorkingTreeEntry {
                                                file_icon: (!entry.is_dir)
                                                    .then(|| file_icon_for_name(&entry.name)),
                                                relative_path: entry.relative_path,
                                                absolute_path: entry.absolute_path,
                                                name: entry.name,
                                                is_dir: entry.is_dir,
                                                expanded: entry.expanded,
                                                depth: entry.depth,
                                            })
                                            .collect()
                                    }
                                    Ok(_) | Err(_) => Vec::new(),
                                }
                            }
                        })
                        .await;
                    waku.update(cx, |waku, cx| {
                        if waku.working_trees.fulfill(token, entries.clone())
                            && waku.selected_panel_scope().as_ref() == Some(&scope)
                        {
                            waku.right_panel_working_tree = entries;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    fn latest_review_turn_source(&self) -> Option<ReviewDiffSource> {
        let session = self.selected_session()?;
        session
            .turns
            .iter()
            .rev()
            .find(|turn| {
                turn.turn_count > 0
                    && turn
                        .checkpoint
                        .as_ref()
                        .is_some_and(|checkpoint| checkpoint.status == CheckpointStatus::Ready)
            })
            .map(|turn| ReviewDiffSource::LastTurn {
                session_id: session.id,
                turn_id: turn.id,
                turn_count: turn.turn_count,
            })
    }

    fn review_diff_source_label(&self, source: ReviewDiffSource) -> String {
        match source {
            ReviewDiffSource::LastTurn { .. }
                if self.latest_review_turn_source() == Some(source) =>
            {
                tr!("diff.source_last_turn")
            }
            ReviewDiffSource::LastTurn { turn_count, .. } => {
                tr!("diff.source_turn", turn = turn_count)
            }
            ReviewDiffSource::Uncommitted => tr!("diff.source_uncommitted"),
            ReviewDiffSource::Unstaged => tr!("diff.source_unstaged"),
            ReviewDiffSource::Staged => tr!("diff.source_staged"),
            ReviewDiffSource::Committed => tr!("diff.source_committed"),
            ReviewDiffSource::Branch => tr!("diff.source_branch"),
        }
    }

    pub(super) fn set_right_panel_diff_source(
        &mut self,
        source: ReviewDiffSource,
        cx: &mut Context<Self>,
    ) {
        if self.right_panel_diff_source != source {
            self.right_panel_diff_selection.clear();
            self.right_panel_diff_source = source;
            self.right_panel_diff_snapshot = None;
            self.right_panel_diff_error = None;
            self.right_panel_diff_selected_file = None;
            self.right_panel_diff_expanded_paths.clear();
            self.right_panel_diff_tree_cursor = None;
            self.right_panel_diff_tree_rows.borrow_mut().clear();
            self.right_panel_diff_tree_list_state.reset(0);
            self.right_panel_diff_list_state.reset(0);
        }
        self.open_right_panel_surface(RightPanelSurface::Diff, cx);
    }

    /// Captures one stable Git range and turns it into render-ready rows. Git,
    /// patch parsing, and syntax tokenization all stay off the UI thread; the
    /// generation check prevents an old source or session from landing late.
    fn refresh_right_panel_diff(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.state.selected_session else {
            self.right_panel_diff_selection.clear();
            self.right_panel_diff_snapshot = None;
            self.right_panel_diff_loading = false;
            self.right_panel_diff_error = Some(tr!("diff.unavailable"));
            return;
        };
        let Some(project_path) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            self.right_panel_diff_selection.clear();
            self.right_panel_diff_snapshot = None;
            self.right_panel_diff_loading = false;
            self.right_panel_diff_error = Some(tr!("diff.unavailable"));
            return;
        };

        self.right_panel_diff_generation = self.right_panel_diff_generation.wrapping_add(1);
        let generation = self.right_panel_diff_generation;
        let source = self.right_panel_diff_source;
        let had_snapshot = self.right_panel_diff_snapshot.is_some();
        let previous_directories = self
            .right_panel_diff_snapshot
            .as_ref()
            .map_or_else(HashSet::new, |snapshot| {
                review_diff_directory_paths(&snapshot.files)
            });
        let selected_path = self.right_panel_diff_selected_file.and_then(|index| {
            self.right_panel_diff_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.files.get(index))
                .map(|file| file.path.clone())
        });
        self.right_panel_diff_loading = true;
        self.right_panel_diff_error = None;
        cx.notify();

        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let project_path = project_path.clone();
                    async move {
                        match workspace.request(
                            waku_client::WorkspaceOperation::CollectReviewDiff {
                                cwd: project_path,
                                source: crate::review_diff::wire_source(source),
                            },
                        )? {
                            waku_client::WorkspaceResult::ReviewDiff { data } => {
                                Ok(crate::review_diff::parse_collected(
                                    source,
                                    &data.numstat,
                                    &data.patch,
                                    data.complete_context,
                                ))
                            }
                            _ => anyhow::bail!("the daemon returned an invalid diff response"),
                        }
                    }
                })
                .await;
            waku.update(cx, |waku, cx| {
                let still_current = waku.state.selected_session == Some(session_id)
                    && waku.right_panel_diff_generation == generation
                    && waku.right_panel_diff_source == source
                    && waku
                        .selected_workspace_path()
                        .is_some_and(|path| path == project_path);
                if !still_current {
                    return;
                }

                waku.right_panel_diff_loading = false;
                match result {
                    Ok(snapshot) => {
                        waku.right_panel_diff_selection.clear();
                        let directories = review_diff_directory_paths(&snapshot.files);
                        if had_snapshot {
                            waku.right_panel_diff_expanded_paths
                                .retain(|path| directories.contains(path));
                            waku.right_panel_diff_expanded_paths
                                .extend(directories.difference(&previous_directories).cloned());
                        } else {
                            waku.right_panel_diff_expanded_paths = directories;
                        }
                        waku.right_panel_diff_selected_file = selected_path
                            .as_deref()
                            .and_then(|path| {
                                snapshot.files.iter().position(|file| file.path == path)
                            })
                            .or_else(|| (!snapshot.files.is_empty()).then_some(0));
                        let line_count = snapshot.lines.len();
                        waku.right_panel_diff_snapshot = Some(Arc::new(snapshot));
                        waku.right_panel_diff_error = None;
                        waku.right_panel_diff_list_state.reset(line_count);
                        waku.sync_right_panel_diff_tree_rows(cx);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if waku.right_panel_diff_snapshot.is_some() {
                            waku.show_toast(tr!("diff.refresh_failed", error = message));
                        } else {
                            waku.right_panel_diff_error = Some(message);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn sync_right_panel_diff_tree_rows(&mut self, cx: &mut Context<Self>) {
        let filter = self.right_panel_diff_filter.read(cx).content().to_owned();
        let previous_cursor_row = self
            .right_panel_diff_tree_cursor
            .and_then(|index| self.right_panel_diff_tree_rows.borrow().get(index).cloned());
        let rows = self
            .right_panel_diff_snapshot
            .as_ref()
            .map_or_else(Vec::new, |snapshot| {
                review_diff_tree_rows(
                    &snapshot.files,
                    &self.right_panel_diff_expanded_paths,
                    &filter,
                )
            });
        let cursor = previous_cursor_row
            .as_ref()
            .and_then(|previous| {
                rows.iter().position(|row| match (previous, row) {
                    (
                        ReviewDiffTreeRow::Directory { path: left, .. },
                        ReviewDiffTreeRow::Directory { path: right, .. },
                    ) => left == right,
                    (
                        ReviewDiffTreeRow::File {
                            file_index: left, ..
                        },
                        ReviewDiffTreeRow::File {
                            file_index: right, ..
                        },
                    ) => left == right,
                    _ => false,
                })
            })
            .or_else(|| {
                self.right_panel_diff_selected_file.and_then(|selected| {
                    rows.iter().position(|row| {
                        matches!(
                            row,
                            ReviewDiffTreeRow::File { file_index, .. }
                                if *file_index == selected
                        )
                    })
                })
            })
            .or_else(|| (!rows.is_empty()).then_some(0));
        let row_count = rows.len();
        *self.right_panel_diff_tree_rows.borrow_mut() = rows;
        self.right_panel_diff_tree_cursor = cursor;
        self.right_panel_diff_tree_list_state
            .reset_with_uniform_height(row_count, px(30.0));
    }

    fn toggle_right_panel_diff_directory(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.right_panel_diff_expanded_paths.remove(&path) {
            self.right_panel_diff_expanded_paths.insert(path);
        }
        self.sync_right_panel_diff_tree_rows(cx);
        cx.notify();
    }

    fn select_right_panel_diff_file(&mut self, file_index: usize, cx: &mut Context<Self>) {
        self.right_panel_diff_selected_file = Some(file_index);
        if let Some(line) = self
            .right_panel_diff_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.files.get(file_index))
            .and_then(|file| file.diff_line)
        {
            // `scroll_to_reveal_item` bottom-aligns targets below the viewport,
            // which can reveal only the file header and leave its diff body
            // off-screen. A tree selection is an explicit jump, so top-anchor
            // the header and expose the content immediately below it.
            self.right_panel_diff_list_state
                .scroll_to(gpui::ListOffset {
                    item_ix: line,
                    offset_in_item: px(0.0),
                });
        }
        cx.notify();
    }

    fn right_panel_diff_tree_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.right_panel_diff_tree_rows.borrow().clone();
        if rows.is_empty() {
            return;
        }
        let current = self
            .right_panel_diff_tree_cursor
            .filter(|index| *index < rows.len())
            .unwrap_or(0);
        let key = event.keystroke.key.as_str();
        let target = match key {
            "up" => Some(current.saturating_sub(1)),
            "down" => Some((current + 1).min(rows.len() - 1)),
            "home" => Some(0),
            "end" => Some(rows.len() - 1),
            "left" => match &rows[current] {
                ReviewDiffTreeRow::Directory {
                    path,
                    expanded: true,
                    ..
                } => {
                    self.toggle_right_panel_diff_directory(path.clone(), cx);
                    None
                }
                ReviewDiffTreeRow::Directory { depth, .. }
                | ReviewDiffTreeRow::File { depth, .. } => {
                    rows[..current].iter().rposition(|row| {
                        matches!(
                            row,
                            ReviewDiffTreeRow::Directory {
                                depth: parent_depth,
                                ..
                            } if *parent_depth < *depth
                        )
                    })
                }
            },
            "right" => match &rows[current] {
                ReviewDiffTreeRow::Directory {
                    path,
                    expanded: false,
                    ..
                } => {
                    self.toggle_right_panel_diff_directory(path.clone(), cx);
                    None
                }
                ReviewDiffTreeRow::Directory { depth, .. }
                    if rows.get(current + 1).is_some_and(|row| match row {
                        ReviewDiffTreeRow::Directory {
                            depth: child_depth, ..
                        }
                        | ReviewDiffTreeRow::File {
                            depth: child_depth, ..
                        } => child_depth > depth,
                    }) =>
                {
                    Some(current + 1)
                }
                _ => None,
            },
            "enter" | "space" => {
                match &rows[current] {
                    ReviewDiffTreeRow::Directory { path, .. } => {
                        self.toggle_right_panel_diff_directory(path.clone(), cx)
                    }
                    ReviewDiffTreeRow::File { file_index, .. } => {
                        self.select_right_panel_diff_file(*file_index, cx)
                    }
                }
                None
            }
            _ => return,
        };
        if let Some(target) = target {
            self.right_panel_diff_tree_cursor = Some(target);
            self.right_panel_diff_tree_list_state
                .scroll_to_reveal_item(target);
            cx.notify();
        }
        cx.stop_propagation();
    }
}
