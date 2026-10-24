//! Git Graph interaction: inline file diffs, the compare selection, context
//! menus, and the dialogs that guard actions which rewrite history.
//!
//! Every `git` call runs on the daemon through a background task. Render only
//! reads the query caches below; a miss starts one fetch and draws a loading
//! line until it lands.

use std::sync::Arc;

use gpui::{KeyBinding, actions};
use waku_client::git::{
    GitDiffSpec, GitGraphAction, GitGraphFile, GitGraphRef, GitGraphRefKind, GitResetMode,
};

use super::right_panel::{DiffRowStyle, render_diff_code_row};
use super::*;

const DIALOG_CONTEXT: &str = "GitGraphDialog";
const DIALOG_FIELD_CONTEXT: &str = "GitGraphDialog > TextInput";
/// Rows drawn for one expanded file. The detail pane is not virtualized, so a
/// generated file must not turn into thousands of elements per frame.
const MAX_INLINE_LINES: usize = 800;
const FILE_ROW_HEIGHT: f32 = 22.0;

actions!(
    waku_git_graph_dialog,
    [ConfirmGitGraphDialog, DismissGitGraphDialog]
);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "secondary-enter",
            ConfirmGitGraphDialog,
            Some(DIALOG_CONTEXT),
        ),
        KeyBinding::new("escape", DismissGitGraphDialog, Some(DIALOG_CONTEXT)),
        KeyBinding::new("escape", DismissGitGraphDialog, Some(DIALOG_FIELD_CONTEXT)),
    ]);
}

/// A parsed, syntax-tokenized patch for one file, built off the UI thread.
pub(super) struct InlineDiff {
    lines: Vec<crate::review_diff::Line>,
    truncated: bool,
}

type FileDiffKey = (PathBuf, GitDiffSpec, String);

pub(super) struct GitGraphUi {
    /// Files opened in the detail pane, for the diff spec in `expanded_spec`.
    /// Selecting something else closes them.
    expanded: HashSet<String>,
    expanded_spec: Option<(PathBuf, GitDiffSpec)>,
    file_diffs: QueryCache<FileDiffKey, Result<InlineDiff, String>>,
    /// The last diff drawn per expanded file, kept while an uncommitted diff
    /// refetches so the pane does not flash a loading line on every refresh.
    shown_diffs: HashMap<String, Arc<Result<InlineDiff, String>>>,
    compare_files: QueryCache<(PathBuf, String, String), Result<Vec<GitGraphFile>, String>>,
    /// Second commit picked with the primary modifier; the detail pane then
    /// compares it with the selected row.
    compare: Option<(PathBuf, String)>,
    dialog: Option<GitGraphDialog>,
    name_input: Entity<TextInput>,
    message_input: Entity<TextInput>,
    selection: TranscriptSelection,
    pending: bool,
}

impl GitGraphUi {
    pub(super) fn new(window: &mut Window, cx: &mut App) -> Self {
        let name_input = cx.new(|cx| TextInput::new(window, cx));
        let message_input = cx
            .new(|cx| TextInput::new(window, cx).placeholder(tr!("git_graph.dialog.tag_message")));
        Self {
            expanded: HashSet::new(),
            expanded_spec: None,
            file_diffs: QueryCache::new(64),
            shown_diffs: HashMap::new(),
            compare_files: QueryCache::new(16),
            compare: None,
            dialog: None,
            name_input,
            message_input,
            selection: TranscriptSelection::default(),
            pending: false,
        }
    }

    /// Enter in either dialog field confirms the dialog.
    pub(super) fn subscribe(&self, cx: &mut Context<Waku>) {
        for input in [&self.name_input, &self.message_input] {
            cx.subscribe(input, |this: &mut Waku, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Submit(_)) && this.git_graph_ui.dialog.is_some() {
                    this.confirm_git_graph_dialog(cx);
                }
            })
            .detach();
        }
    }

    pub(super) fn clear(&mut self) {
        self.expanded.clear();
        self.expanded_spec = None;
        self.file_diffs.clear();
        self.shown_diffs.clear();
        self.compare_files.clear();
        self.compare = None;
    }

    pub(super) fn pending(&self) -> bool {
        self.pending
    }
}

pub(super) struct GitGraphDialog {
    kind: GitGraphDialogKind,
    cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    /// "Check out after creating" or "force delete", depending on `kind`.
    toggle: bool,
}

pub(super) enum GitGraphDialogKind {
    Confirm {
        title: String,
        message: String,
        confirm: String,
        action: GitGraphAction,
    },
    DeleteBranch {
        name: String,
    },
    CreateBranch {
        start: String,
        label: String,
    },
    CreateTag {
        target: String,
        label: String,
    },
    RenameBranch {
        from: String,
    },
}

impl GitGraphDialogKind {
    fn has_input(&self) -> bool {
        matches!(
            self,
            Self::CreateBranch { .. } | Self::CreateTag { .. } | Self::RenameBranch { .. }
        )
    }

    fn danger(&self) -> bool {
        matches!(self, Self::Confirm { .. } | Self::DeleteBranch { .. })
    }
}

/// What a commit row's menu acts on.
#[derive(Clone)]
pub(super) struct CommitMenuTarget {
    pub(super) index: usize,
    pub(super) hash: String,
    pub(super) subject: String,
    /// The selected commit, when it is a different row this one can be
    /// compared with.
    pub(super) compare_with: Option<String>,
}

impl Waku {
    // ── Diff spec and compare selection ─────────────────────────────────

    /// What the detail pane compares for the selected row.
    pub(super) fn git_graph_detail_spec(
        &self,
        index: usize,
        snapshot: &waku_client::git::GitGraphSnapshot,
    ) -> Option<GitDiffSpec> {
        let uncommitted = !snapshot.uncommitted.is_empty();
        if uncommitted && index == 0 {
            return Some(GitDiffSpec::Uncommitted);
        }
        let position = index - usize::from(uncommitted);
        let commit = snapshot.commits.get(position)?;
        let path = self.selected_workspace_path()?;
        let other = self
            .git_graph_ui
            .compare
            .as_ref()
            .filter(|(compare_path, hash)| compare_path == path && *hash != commit.hash)
            .and_then(|(_, hash)| {
                snapshot
                    .commits
                    .iter()
                    .position(|candidate| &candidate.hash == hash)
            });
        Some(match other {
            // Newest-first: the row further down is the older side.
            Some(other) if other > position => GitDiffSpec::Compare {
                from: snapshot.commits[other].hash.clone(),
                to: commit.hash.clone(),
            },
            Some(other) => GitDiffSpec::Compare {
                from: commit.hash.clone(),
                to: snapshot.commits[other].hash.clone(),
            },
            None => GitDiffSpec::Commit {
                hash: commit.hash.clone(),
            },
        })
    }

    pub(super) fn git_graph_compare_hash(&self) -> Option<&str> {
        let path = self.selected_workspace_path()?;
        self.git_graph_ui
            .compare
            .as_ref()
            .filter(|(compare_path, _)| compare_path == path)
            .map(|(_, hash)| hash.as_str())
    }

    /// Primary-modifier click: compare `hash` with the selected row, or drop
    /// the comparison when the same commit is picked again.
    pub(super) fn toggle_git_graph_compare(&mut self, hash: String, cx: &mut Context<Self>) {
        let Some(path) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return;
        };
        if self.git_graph_compare_hash() == Some(hash.as_str()) {
            self.git_graph_ui.compare = None;
        } else {
            self.git_graph_ui.compare = Some((path, hash));
        }
        self.git_graph_detail_scroll
            .set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    pub(super) fn clear_git_graph_compare(&mut self) -> bool {
        self.git_graph_ui.compare.take().is_some()
    }

    fn git_graph_compare_files(
        &mut self,
        path: &Path,
        from: &str,
        to: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Result<Vec<GitGraphFile>, String>>> {
        let key = (path.to_path_buf(), from.to_owned(), to.to_owned());
        match self.git_graph_ui.compare_files.read(&key) {
            Query::Ready(value) => Some(value),
            Query::Pending => None,
            Query::Missing(token) => {
                let workspace =
                    waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
                let (cwd, from, to) = key;
                cx.spawn(async move |waku, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            match workspace.request(
                                waku_client::WorkspaceOperation::InspectGitCompare {
                                    cwd,
                                    from,
                                    to,
                                },
                            ) {
                                Ok(waku_client::WorkspaceResult::GitCompare { files }) => Ok(files),
                                Ok(_) => {
                                    Err("the daemon returned an invalid compare response"
                                        .to_owned())
                                }
                                Err(error) => Err(error.to_string()),
                            }
                        })
                        .await;
                    let _ = waku.update(cx, |waku, cx| {
                        if waku.git_graph_ui.compare_files.fulfill(token, result) {
                            cx.notify();
                        }
                    });
                })
                .detach();
                None
            }
        }
    }

    // ── Inline file diffs ───────────────────────────────────────────────

    /// Called once per detail render: a different spec closes every file
    /// opened for the previous one.
    fn sync_git_graph_expanded(&mut self, path: &Path, spec: &GitDiffSpec) {
        let current = self
            .git_graph_ui
            .expanded_spec
            .as_ref()
            .is_some_and(|(expanded_path, expanded)| expanded_path == path && expanded == spec);
        if !current {
            self.git_graph_ui.expanded.clear();
            self.git_graph_ui.shown_diffs.clear();
            self.git_graph_ui.expanded_spec = Some((path.to_path_buf(), spec.clone()));
        }
    }

    fn toggle_git_graph_file(&mut self, file: String, cx: &mut Context<Self>) {
        if !self.git_graph_ui.expanded.remove(&file) {
            self.git_graph_ui.expanded.insert(file);
        } else {
            self.git_graph_ui.shown_diffs.remove(&file);
        }
        cx.notify();
    }

    /// A fresh graph means the working tree may have moved; refetch the
    /// uncommitted diffs that are open. Commit diffs never change.
    pub(super) fn invalidate_git_graph_uncommitted_diffs(&mut self, path: &Path) {
        let Some((expanded_path, GitDiffSpec::Uncommitted)) = &self.git_graph_ui.expanded_spec
        else {
            return;
        };
        if expanded_path != path {
            return;
        }
        for file in &self.git_graph_ui.expanded {
            self.git_graph_ui.file_diffs.invalidate(&(
                path.to_path_buf(),
                GitDiffSpec::Uncommitted,
                file.clone(),
            ));
        }
    }

    fn git_graph_file_diff(
        &mut self,
        path: &Path,
        spec: &GitDiffSpec,
        file: &GitGraphFile,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Result<InlineDiff, String>>> {
        let key = (path.to_path_buf(), spec.clone(), file.path.clone());
        let value = match self.git_graph_ui.file_diffs.read(&key) {
            Query::Ready(value) => Some(value),
            Query::Pending => None,
            Query::Missing(token) => {
                self.spawn_git_graph_file_diff(key, file.old_path.clone(), token, cx);
                None
            }
        };
        match value {
            Some(value) => {
                self.git_graph_ui
                    .shown_diffs
                    .insert(file.path.clone(), Arc::clone(&value));
                Some(value)
            }
            None => self.git_graph_ui.shown_diffs.get(&file.path).cloned(),
        }
    }

    fn spawn_git_graph_file_diff(
        &self,
        key: FileDiffKey,
        old_path: Option<String>,
        token: crate::query::FetchToken<FileDiffKey>,
        cx: &mut Context<Self>,
    ) {
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        let (cwd, spec, path) = key;
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let diff = match workspace.request(
                        waku_client::WorkspaceOperation::InspectGitFileDiff {
                            cwd,
                            spec,
                            path,
                            old_path,
                        },
                    ) {
                        Ok(waku_client::WorkspaceResult::GitFileDiff { diff }) => diff,
                        Ok(_) => {
                            return Err("the daemon returned an invalid diff response".to_owned());
                        }
                        Err(error) => return Err(error.to_string()),
                    };
                    let parsed = crate::review_diff::parse_collected(
                        ReviewDiffSource::default(),
                        "",
                        &diff.patch,
                        false,
                    );
                    let mut lines = parsed
                        .lines
                        .into_iter()
                        .filter(|line| line.kind != crate::review_diff::LineKind::FileHeader)
                        .collect::<Vec<_>>();
                    let truncated = diff.truncated || lines.len() > MAX_INLINE_LINES;
                    lines.truncate(MAX_INLINE_LINES);
                    Ok(InlineDiff { lines, truncated })
                })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                if waku.git_graph_ui.file_diffs.fulfill(token, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The detail pane for the selected row: header, file list, and any
    /// files opened inline.
    pub(super) fn render_git_graph_detail(
        &mut self,
        index: usize,
        snapshot: &waku_client::git::GitGraphSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let Some(path) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return div().id("git-graph-detail-empty");
        };
        let Some(spec) = self.git_graph_detail_spec(index, snapshot) else {
            return div().id("git-graph-detail-empty");
        };
        self.sync_git_graph_expanded(&path, &spec);
        let expanded = !self.git_graph_ui.expanded.is_empty();
        let mut body = div()
            .id("git-graph-detail-scroll")
            .overflow_y_scroll()
            .track_scroll(&self.git_graph_detail_scroll)
            .border_t_1()
            .border_color(theme.border)
            // An open diff needs room; split the panel with the list instead
            // of squeezing the patch into the summary's height.
            .when(expanded, |element| element.flex_1().min_h(px(180.0)))
            .when(!expanded, |element| element.flex_none().max_h(px(260.0)))
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(md::render::frame_reset(self.git_graph_ui.selection.clone()));

        let files: Result<Option<Vec<GitGraphFile>>, String> = match &spec {
            GitDiffSpec::Uncommitted => {
                body = body.child(
                    div()
                        .text_size(sp(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr!("git_graph.uncommitted")),
                );
                Ok(Some(snapshot.uncommitted.clone()))
            }
            GitDiffSpec::Commit { hash } => {
                let commit = snapshot
                    .commits
                    .iter()
                    .find(|commit| &commit.hash == hash)
                    .cloned();
                let Some(commit) = commit else {
                    return body;
                };
                body = self.render_git_graph_commit_header(body, &commit, theme, cx);
                match self.git_commit_detail(hash, cx).as_deref() {
                    Some(Ok(detail)) => {
                        if let Some(extra) =
                            super::git_graph::message_body(&commit.subject, &detail.body)
                        {
                            body = body.child(
                                div()
                                    .text_size(sp(12.0))
                                    .line_height(sp(17.0))
                                    .text_color(theme.text_secondary)
                                    .whitespace_normal()
                                    .child(extra.to_owned()),
                            );
                        }
                        Ok(Some(detail.files.clone()))
                    }
                    Some(Err(error)) => Err(error.clone()),
                    None => Ok(None),
                }
            }
            GitDiffSpec::Compare { from, to } => {
                let subject = |hash: &str| {
                    snapshot
                        .commits
                        .iter()
                        .find(|commit| commit.hash == hash)
                        .map(|commit| commit.subject.clone())
                        .unwrap_or_default()
                };
                body = body
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(icon("icons/file-diff.svg", 13.0, theme.text_secondary))
                            .child(tr!(
                                "git_graph.compare_title",
                                from = super::git_graph::short_hash(from),
                                to = super::git_graph::short_hash(to)
                            )),
                    )
                    .child(compare_side(
                        tr!("git_graph.compare_from"),
                        subject(from),
                        theme,
                    ))
                    .child(compare_side(
                        tr!("git_graph.compare_to"),
                        subject(to),
                        theme,
                    ))
                    .child(
                        div()
                            .text_size(sp(11.5))
                            .text_color(theme.text_tertiary)
                            .child(tr!("git_graph.compare_hint")),
                    );
                match self.git_graph_compare_files(&path, from, to, cx).as_deref() {
                    Some(Ok(files)) => Ok(Some(files.clone())),
                    Some(Err(error)) => Err(error.clone()),
                    None => Ok(None),
                }
            }
        };

        body = match files {
            Ok(Some(files)) => {
                body.child(self.render_git_graph_files(&path, &spec, &files, theme, cx))
            }
            Ok(None) => body.child(
                div()
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("git_graph.loading")),
            ),
            Err(error) => body.child(
                div()
                    .text_size(sp(12.0))
                    .text_color(theme.danger)
                    .whitespace_normal()
                    .child(error),
            ),
        };
        let selection = self.git_graph_ui.selection.clone();
        body.child(
            canvas(
                |_, _, _| (),
                move |_, _, window, _| md::render::install_selection_input(window, &selection),
            )
            .absolute()
            .w(px(0.0))
            .h(px(0.0)),
        )
    }

    fn render_git_graph_commit_header(
        &self,
        mut body: Stateful<Div>,
        commit: &waku_client::git::GitGraphCommit,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let authored = DateTime::<Utc>::from_timestamp(commit.authored_at, 0)
            .map(|time| {
                time.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        body = body
            .child(
                div()
                    .text_size(sp(12.5))
                    .line_height(sp(17.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .whitespace_normal()
                    .child(commit.subject.clone()),
            )
            .child(
                div()
                    .text_size(sp(11.5))
                    .text_color(theme.text_tertiary)
                    .child(format!("{} · {}", commit.author, authored)),
            )
            .child(self.render_git_graph_hash(&commit.hash, theme, cx));
        if !commit.parents.is_empty() {
            body = body.child(
                div()
                    .text_size(sp(11.5))
                    .text_color(theme.text_secondary)
                    .child(format!(
                        "{} {}",
                        tr!("git_graph.parents"),
                        super::git_graph::short_hashes(&commit.parents)
                    )),
            );
        }
        body
    }

    fn render_git_graph_files(
        &mut self,
        path: &Path,
        spec: &GitDiffSpec,
        files: &[GitGraphFile],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        if files.is_empty() {
            return div()
                .text_size(sp(12.0))
                .text_color(theme.text_tertiary)
                .child(tr!("git_graph.no_files"));
        }
        let mut list = div().flex().flex_col().gap(px(1.0)).min_w_0();
        for (position, file) in files.iter().enumerate() {
            let open = self.git_graph_ui.expanded.contains(&file.path);
            list = list.child(self.render_git_graph_file_row(position, file, open, theme, cx));
            if open {
                let diff = self.git_graph_file_diff(path, spec, file, cx);
                list = list.child(self.render_git_graph_inline_diff(position, diff, theme));
            }
        }
        list
    }

    fn render_git_graph_file_row(
        &self,
        position: usize,
        file: &GitGraphFile,
        open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let color = match file.status {
            waku_client::git::GitGraphFileStatus::Added => theme.success,
            waku_client::git::GitGraphFileStatus::Deleted => theme.danger,
            waku_client::git::GitGraphFileStatus::Modified
            | waku_client::git::GitGraphFileStatus::TypeChanged => theme.warning,
            _ => theme.text_secondary,
        };
        let label = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path),
            None => file.path.clone(),
        };
        let focus = self.transcript_control_focus(format!("git-graph-file-{position}"), cx);
        let click_path = file.path.clone();
        let key_path = file.path.clone();
        div()
            .id(SharedString::from(format!("git-graph-file-{position}")))
            .track_focus(&focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(FILE_ROW_HEIGHT))
            .px(px(4.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .min_w_0()
            .cursor_default()
            .when(open, |element| element.bg(theme.overlay))
            .hover(|element| element.bg(theme.overlay))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .tooltip(Tooltip::text(if open {
                tr!("git_graph.hide_diff")
            } else {
                tr!("git_graph.show_diff")
            }))
            .child(icon(
                if open {
                    "icons/chevron-down.svg"
                } else {
                    "icons/chevron-right.svg"
                },
                11.0,
                theme.text_tertiary,
            ))
            .child(
                div()
                    .w(px(12.0))
                    .flex_none()
                    .text_size(sp(11.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color)
                    .child(super::git_graph::status_letter(file.status)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(sp(12.0))
                    .text_color(theme.text)
                    .truncate()
                    .child(label),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_git_graph_file(click_path.clone(), cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_git_graph_file(key_path.clone(), cx);
                    cx.stop_propagation();
                }
            }))
    }

    fn render_git_graph_inline_diff(
        &self,
        position: usize,
        diff: Option<Arc<Result<InlineDiff, String>>>,
        theme: &Theme,
    ) -> AnyElement {
        let frame = div()
            .w_full()
            .min_w_0()
            .mb(px(4.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .flex()
            .flex_col();
        let Some(diff) = diff else {
            return frame
                .child(diff_caption(tr!("git_graph.loading"), theme))
                .into_any_element();
        };
        let diff = match diff.as_ref() {
            Ok(diff) => diff,
            Err(error) => {
                return frame
                    .child(
                        div()
                            .px(px(10.0))
                            .py(px(6.0))
                            .text_size(sp(12.0))
                            .text_color(theme.danger)
                            .whitespace_normal()
                            .child(error.clone()),
                    )
                    .into_any_element();
            }
        };
        if diff.lines.is_empty() {
            return frame
                .child(diff_caption(tr!("git_graph.no_textual_changes"), theme))
                .into_any_element();
        }
        let style = DiffRowStyle::review(self.state.code_font_size);
        let key = format!("git-graph-diff-{position}");
        let mut column = frame
            .font_family(md::render::MONO_FAMILY)
            .text_size(px(self.state.code_font_size));
        for (index, line) in diff.lines.iter().enumerate() {
            column = column.child(match &line.kind {
                crate::review_diff::LineKind::Context
                | crate::review_diff::LineKind::Addition
                | crate::review_diff::LineKind::Deletion => render_diff_code_row(
                    line,
                    index,
                    &key,
                    &self.git_graph_ui.selection,
                    style,
                    theme,
                ),
                crate::review_diff::LineKind::Gap(gap) => {
                    diff_caption(tr!("diff.unmodified_lines", count = gap.count()), theme)
                        .into_any_element()
                }
                _ => diff_caption(line.content.clone(), theme).into_any_element(),
            });
        }
        if diff.truncated {
            column = column.child(diff_caption(tr!("git_graph.diff_truncated"), theme));
        }
        column.into_any_element()
    }

    // ── Actions ─────────────────────────────────────────────────────────

    pub(super) fn run_git_graph_action(&mut self, action: GitGraphAction, cx: &mut Context<Self>) {
        if self.git_graph_ui.pending {
            return;
        }
        let Some(path) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return;
        };
        self.git_graph_ui.pending = true;
        cx.notify();
        let success = action_success_message(&action);
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        match workspace.request(
                            waku_client::WorkspaceOperation::RunGitGraphAction {
                                cwd: path,
                                action,
                            },
                        ) {
                            Ok(waku_client::WorkspaceResult::Branches { snapshot }) => Ok(snapshot),
                            Ok(_) => Err("the daemon returned an invalid git response".to_owned()),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                waku.git_graph_ui.pending = false;
                match result {
                    Ok(snapshot) => {
                        match snapshot {
                            Some(snapshot) => waku.apply_branch_change(&path, snapshot, cx),
                            None => {
                                waku.git_graph_snapshots.invalidate(&path);
                                waku.invalidate_workspace_queries(cx);
                            }
                        }
                        waku.show_success_toast(success);
                    }
                    Err(error) => {
                        // A failed merge or rebase can still leave the tree
                        // changed (conflicts), so redraw from Git either way.
                        waku.git_graph_snapshots.invalidate(&path);
                        waku.invalidate_workspace_queries(cx);
                        waku.show_toast(tr!("git_graph.action_failed", error = error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_git_graph_text(&mut self, text: String, message: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.show_success_toast(message);
        cx.notify();
    }

    // ── Dialog ──────────────────────────────────────────────────────────

    pub(super) fn open_git_graph_dialog(
        &mut self,
        kind: GitGraphDialogKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git_graph_ui.pending {
            return;
        }
        let cancel_focus = cx.focus_handle();
        let confirm_focus = cx.focus_handle();
        let (prefill, placeholder) = match &kind {
            GitGraphDialogKind::CreateBranch { .. } => {
                (String::new(), tr!("git_graph.dialog.branch_name"))
            }
            GitGraphDialogKind::CreateTag { .. } => {
                (String::new(), tr!("git_graph.dialog.tag_name"))
            }
            GitGraphDialogKind::RenameBranch { from } => {
                (from.clone(), tr!("git_graph.dialog.branch_name"))
            }
            _ => (String::new(), String::new()),
        };
        self.git_graph_ui.name_input.update(cx, |input, cx| {
            input.set_placeholder(placeholder, cx);
            input.set_content(prefill, cx);
            input.select_all_text(cx);
        });
        self.git_graph_ui
            .message_input
            .update(cx, |input, cx| input.clear(cx));
        let focus = if kind.has_input() {
            self.git_graph_ui.name_input.read(cx).focus_handle(cx)
        } else {
            cancel_focus.clone()
        };
        self.git_graph_ui.dialog = Some(GitGraphDialog {
            toggle: matches!(kind, GitGraphDialogKind::CreateBranch { .. }),
            kind,
            cancel_focus,
            confirm_focus,
        });
        // The dialog joins the dispatch tree only after it has drawn.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
        cx.notify();
    }

    fn dismiss_git_graph_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git_graph_ui.dialog.take().is_some() {
            window.focus(&self.git_graph_focus, cx);
            cx.notify();
        }
    }

    pub(super) fn confirm_git_graph_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.git_graph_ui.dialog.take() else {
            return;
        };
        let name = self
            .git_graph_ui
            .name_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let action = match &dialog.kind {
            GitGraphDialogKind::Confirm { action, .. } => Some(action.clone()),
            GitGraphDialogKind::DeleteBranch { name } => Some(GitGraphAction::DeleteBranch {
                name: name.clone(),
                force: dialog.toggle,
            }),
            GitGraphDialogKind::CreateBranch { start, .. } => {
                (!name.is_empty()).then(|| GitGraphAction::CreateBranch {
                    name: name.clone(),
                    start: start.clone(),
                    checkout: dialog.toggle,
                })
            }
            GitGraphDialogKind::CreateTag { target, .. } => {
                let message = self.git_graph_ui.message_input.read(cx).content().trim();
                (!name.is_empty()).then(|| GitGraphAction::CreateTag {
                    name: name.clone(),
                    target: target.clone(),
                    message: (!message.is_empty()).then(|| message.to_owned()),
                })
            }
            GitGraphDialogKind::RenameBranch { from } => {
                (!name.is_empty() && &name != from).then(|| GitGraphAction::RenameBranch {
                    from: from.clone(),
                    to: name.clone(),
                })
            }
        };
        match action {
            Some(action) => self.run_git_graph_action(action, cx),
            // An empty name keeps the dialog up; an unchanged rename is done.
            None if name.is_empty() && dialog.kind.has_input() => {
                self.git_graph_ui.dialog = Some(dialog);
            }
            None => {}
        }
        cx.notify();
    }

    pub(super) fn render_git_graph_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.git_graph_ui.dialog.as_ref()?;
        let theme = Theme::current(cx);
        let danger = dialog.kind.danger();
        let (title, message, confirm_label, toggle_label): (
            String,
            Option<String>,
            String,
            Option<String>,
        ) = match &dialog.kind {
            GitGraphDialogKind::Confirm {
                title,
                message,
                confirm,
                ..
            } => (title.clone(), Some(message.clone()), confirm.clone(), None),
            GitGraphDialogKind::DeleteBranch { name } => (
                tr!("git_graph.dialog.delete_branch_title"),
                Some(tr!("git_graph.dialog.delete_branch_message", name = name)),
                tr!("git_graph.menu.delete"),
                Some(tr!("git_graph.dialog.force_delete")),
            ),
            GitGraphDialogKind::CreateBranch { label, .. } => (
                tr!("git_graph.dialog.create_branch_title"),
                Some(tr!("git_graph.dialog.create_at", target = label)),
                tr!("git_graph.dialog.create"),
                Some(tr!("git_graph.dialog.checkout_after_create")),
            ),
            GitGraphDialogKind::CreateTag { label, .. } => (
                tr!("git_graph.dialog.create_tag_title"),
                Some(tr!("git_graph.dialog.create_at", target = label)),
                tr!("git_graph.dialog.create"),
                None,
            ),
            GitGraphDialogKind::RenameBranch { from } => (
                tr!("git_graph.dialog.rename_branch_title"),
                Some(tr!("git_graph.dialog.rename_branch_message", name = from)),
                tr!("common.rename"),
                None,
            ),
        };
        let show_name = dialog.kind.has_input();
        let show_message = matches!(dialog.kind, GitGraphDialogKind::CreateTag { .. });
        let toggle_on = dialog.toggle;
        let accent = if danger { theme.danger } else { theme.accent };

        let cancel = div()
            .id("git-graph-dialog-cancel")
            .track_focus(&dialog.cancel_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .h(px(32.0))
            .px(px(14.0))
            .rounded(px(8.0))
            .flex()
            .items_center()
            .cursor_default()
            .text_size(sp(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_secondary)
            .bg(theme.overlay)
            .hover(|element| element.bg(theme.overlay_strong))
            .child(tr_cow!("common.cancel"))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.dismiss_git_graph_dialog(window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                this.dismiss_git_graph_dialog(window, cx);
            }));
        let confirm = div()
            .id("git-graph-dialog-confirm")
            .track_focus(&dialog.confirm_focus)
            .tab_index(0)
            .focus_visible(move |style| style.border_1().border_color(accent))
            .h(px(32.0))
            .px(px(14.0))
            .rounded(px(8.0))
            .flex()
            .items_center()
            .cursor_default()
            .text_size(sp(13.0))
            .font_weight(FontWeight::MEDIUM)
            .when(danger, |element| {
                element
                    .text_color(theme.danger)
                    .bg(theme.danger_soft)
                    .hover(|element| element.bg(theme.danger.opacity(0.16)))
            })
            .when(!danger, |element| {
                element
                    .text_color(theme.on_inverse)
                    .bg(theme.inverse)
                    .hover(|element| element.opacity(0.9))
            })
            .child(confirm_label)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.confirm_git_graph_dialog(cx);
                    cx.stop_propagation();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.confirm_git_graph_dialog(cx);
            }));

        let mut content = div()
            .px(px(16.0))
            .pb(px(14.0))
            .flex()
            .flex_col()
            .gap(px(10.0));
        if let Some(message) = message {
            content = content.child(
                div()
                    .text_size(sp(12.5))
                    .line_height(sp(17.0))
                    .text_color(theme.text_secondary)
                    .whitespace_normal()
                    .child(message),
            );
        }
        if show_name {
            content = content.child(TextField::new(
                "git-graph-dialog-name",
                self.git_graph_ui.name_input.clone(),
            ));
        }
        if show_message {
            content = content.child(TextField::new(
                "git-graph-dialog-message",
                self.git_graph_ui.message_input.clone(),
            ));
        }
        if let Some(toggle_label) = toggle_label {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.0))
                    .text_size(sp(12.5))
                    .text_color(theme.text)
                    .child(toggle_label)
                    .child(toggle_switch(
                        "git-graph-dialog-toggle",
                        toggle_on,
                        false,
                        theme,
                        cx,
                        |this: &mut Waku, _, cx| {
                            if let Some(dialog) = this.git_graph_ui.dialog.as_mut() {
                                dialog.toggle = !dialog.toggle;
                                cx.notify();
                            }
                        },
                    )),
            );
        }

        let card = div()
            .id("git-graph-dialog-card")
            .key_context(DIALOG_CONTEXT)
            .on_action(cx.listener(|this, _: &ConfirmGitGraphDialog, _, cx| {
                this.confirm_git_graph_dialog(cx);
            }))
            .on_action(cx.listener(|this, _: &DismissGitGraphDialog, window, cx| {
                this.dismiss_git_graph_dialog(window, cx);
            }))
            .tab_group()
            .tab_stop(false)
            .w_full()
            .max_w(px(420.0))
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
                    .when(danger, |element| {
                        element.child(icon("icons/alert.svg", 15.0, theme.danger))
                    })
                    .when(!danger, |element| {
                        element.child(icon("icons/git-branch.svg", 15.0, theme.text_secondary))
                    })
                    .child(title),
            )
            .child(content)
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
            .id("git-graph-dialog-layer")
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
                cx.listener(|this, _, window, cx| this.dismiss_git_graph_dialog(window, cx)),
            )
            .child(card);
        Some(gpui::deferred(layer).with_priority(4).into_any_element())
    }

    // ── Context menus ───────────────────────────────────────────────────

    pub(super) fn git_graph_uncommitted_menu(weak: WeakEntity<Waku>) -> Vec<MenuItem> {
        let view = weak.clone();
        let stash = weak.clone();
        let discard = weak;
        vec![
            MenuItem::new(tr!("git_graph.menu.view_changes"), move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.focus_git_graph_row(0, cx);
                    window.focus(&this.git_graph_focus, cx);
                });
            })
            .icon("icons/file-diff.svg"),
            MenuItem::Separator,
            MenuItem::new(tr!("git_graph.menu.stash"), move |_, cx| {
                let _ = stash.update(cx, |this, cx| {
                    this.run_git_graph_action(GitGraphAction::Stash, cx)
                });
            })
            .icon("icons/archive.svg"),
            MenuItem::new(tr!("git_graph.menu.discard"), move |window, cx| {
                let _ = discard.update(cx, |this, cx| {
                    this.open_git_graph_dialog(
                        GitGraphDialogKind::Confirm {
                            title: tr!("git_graph.dialog.discard_title"),
                            message: tr!("git_graph.dialog.discard_message"),
                            confirm: tr!("git_graph.menu.discard_confirm"),
                            action: GitGraphAction::DiscardUncommitted,
                        },
                        window,
                        cx,
                    )
                });
            })
            .icon("icons/trash.svg"),
        ]
    }

    pub(super) fn git_graph_commit_menu(
        weak: WeakEntity<Waku>,
        target: CommitMenuTarget,
    ) -> Vec<MenuItem> {
        let short = super::git_graph::short_hash(&target.hash).to_owned();
        let mut items = Vec::new();
        {
            let weak = weak.clone();
            let index = target.index;
            items.push(
                MenuItem::new(tr!("git_graph.menu.view_changes"), move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.clear_git_graph_compare();
                        this.focus_git_graph_row(index, cx);
                        window.focus(&this.git_graph_focus, cx);
                    });
                })
                .icon("icons/file-diff.svg"),
            );
        }
        if target.compare_with.is_some() {
            let weak = weak.clone();
            let hash = target.hash.clone();
            items.push(
                MenuItem::new(tr!("git_graph.menu.compare_with_selected"), move |_, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.toggle_git_graph_compare(hash.clone(), cx)
                    });
                })
                .icon("icons/split.svg"),
            );
        }
        items.push(MenuItem::Separator);
        {
            let weak = weak.clone();
            let hash = target.hash.clone();
            items.push(
                MenuItem::new(tr!("git_graph.copy_hash"), move |_, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.copy_git_graph_text(hash.clone(), tr!("git_graph.copied"), cx)
                    });
                })
                .icon("icons/copy.svg"),
            );
        }
        {
            let weak = weak.clone();
            let subject = target.subject.clone();
            items.push(MenuItem::new(
                tr!("git_graph.menu.copy_subject"),
                move |_, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.copy_git_graph_text(
                            subject.clone(),
                            tr!("git_graph.copied_subject"),
                            cx,
                        )
                    });
                },
            ));
        }
        items.push(MenuItem::Separator);
        {
            let weak = weak.clone();
            let hash = target.hash.clone();
            let label = short.clone();
            items.push(
                MenuItem::new(tr!("git_graph.menu.create_branch"), move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.open_git_graph_dialog(
                            GitGraphDialogKind::CreateBranch {
                                start: hash.clone(),
                                label: label.clone(),
                            },
                            window,
                            cx,
                        )
                    });
                })
                .icon("icons/git-branch.svg"),
            );
        }
        {
            let weak = weak.clone();
            let hash = target.hash.clone();
            let label = short.clone();
            items.push(MenuItem::new(
                tr!("git_graph.menu.create_tag"),
                move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.open_git_graph_dialog(
                            GitGraphDialogKind::CreateTag {
                                target: hash.clone(),
                                label: label.clone(),
                            },
                            window,
                            cx,
                        )
                    });
                },
            ));
        }
        items.push(action_item(
            &weak,
            tr!("git_graph.menu.checkout_commit"),
            GitGraphAction::CheckoutCommit {
                hash: target.hash.clone(),
            },
        ));
        items.push(MenuItem::Separator);
        items.push(action_item(
            &weak,
            tr!("git_graph.menu.cherry_pick"),
            GitGraphAction::CherryPick {
                hash: target.hash.clone(),
            },
        ));
        items.push(action_item(
            &weak,
            tr!("git_graph.menu.revert"),
            GitGraphAction::Revert {
                hash: target.hash.clone(),
            },
        ));
        items.push(action_item(
            &weak,
            tr!("git_graph.menu.merge_into_current"),
            GitGraphAction::Merge {
                rev: target.hash.clone(),
            },
        ));
        items.push(confirm_item(
            &weak,
            tr!("git_graph.menu.rebase_onto"),
            tr!("git_graph.dialog.rebase_title"),
            tr!("git_graph.dialog.rebase_message", target = short.clone()),
            tr!("git_graph.menu.rebase_confirm"),
            GitGraphAction::Rebase {
                onto: target.hash.clone(),
            },
        ));
        {
            let weak = weak.clone();
            let hash = target.hash.clone();
            let short = short.clone();
            items.push(MenuItem::Submenu {
                label: tr!("git_graph.menu.reset").into(),
                value: None,
                items: Rc::new(move |_| {
                    vec![
                        action_item(
                            &weak,
                            tr!("git_graph.menu.reset_soft"),
                            GitGraphAction::Reset {
                                hash: hash.clone(),
                                mode: GitResetMode::Soft,
                            },
                        ),
                        action_item(
                            &weak,
                            tr!("git_graph.menu.reset_mixed"),
                            GitGraphAction::Reset {
                                hash: hash.clone(),
                                mode: GitResetMode::Mixed,
                            },
                        ),
                        confirm_item(
                            &weak,
                            tr!("git_graph.menu.reset_hard"),
                            tr!("git_graph.dialog.reset_hard_title"),
                            tr!(
                                "git_graph.dialog.reset_hard_message",
                                target = short.clone()
                            ),
                            tr!("git_graph.menu.reset_confirm"),
                            GitGraphAction::Reset {
                                hash: hash.clone(),
                                mode: GitResetMode::Hard,
                            },
                        ),
                    ]
                }),
            });
        }
        items
    }

    /// The menu for one ref capsule: branch, remote branch, tag, or detached
    /// `HEAD`.
    pub(super) fn git_graph_ref_menu(
        weak: WeakEntity<Waku>,
        git_ref: &GitGraphRef,
        hash: &str,
    ) -> Vec<MenuItem> {
        let name = git_ref.name.clone();
        let mut items = Vec::new();
        match git_ref.kind {
            GitGraphRefKind::Branch => {
                if !git_ref.current {
                    items.push(
                        action_item(
                            &weak,
                            tr!("git_graph.menu.checkout_branch"),
                            GitGraphAction::CheckoutBranch { name: name.clone() },
                        )
                        .icon("icons/git-branch.svg"),
                    );
                    items.push(action_item(
                        &weak,
                        tr!(
                            "git_graph.menu.merge_branch_into_current",
                            name = name.clone()
                        ),
                        GitGraphAction::Merge { rev: name.clone() },
                    ));
                    items.push(confirm_item(
                        &weak,
                        tr!("git_graph.menu.rebase_onto_branch", name = name.clone()),
                        tr!("git_graph.dialog.rebase_title"),
                        tr!("git_graph.dialog.rebase_message", target = name.clone()),
                        tr!("git_graph.menu.rebase_confirm"),
                        GitGraphAction::Rebase { onto: name.clone() },
                    ));
                    items.push(MenuItem::Separator);
                }
                items.push(
                    action_item(
                        &weak,
                        tr!("git_graph.menu.push_branch"),
                        GitGraphAction::PushBranch { name: name.clone() },
                    )
                    .icon("icons/cloud-upload.svg"),
                );
                {
                    let weak = weak.clone();
                    let from = name.clone();
                    items.push(
                        MenuItem::new(tr!("git_graph.menu.rename_branch"), move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.open_git_graph_dialog(
                                    GitGraphDialogKind::RenameBranch { from: from.clone() },
                                    window,
                                    cx,
                                )
                            });
                        })
                        .icon("icons/pencil.svg"),
                    );
                }
                if !git_ref.current {
                    let weak = weak.clone();
                    let branch = name.clone();
                    items.push(
                        MenuItem::new(tr!("git_graph.menu.delete_branch"), move |window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.open_git_graph_dialog(
                                    GitGraphDialogKind::DeleteBranch {
                                        name: branch.clone(),
                                    },
                                    window,
                                    cx,
                                )
                            });
                        })
                        .icon("icons/trash.svg"),
                    );
                }
            }
            GitGraphRefKind::Remote => {
                items.push(
                    action_item(
                        &weak,
                        tr!("git_graph.menu.checkout_remote"),
                        GitGraphAction::CheckoutRemoteBranch { name: name.clone() },
                    )
                    .icon("icons/git-branch.svg"),
                );
                items.push(action_item(
                    &weak,
                    tr!(
                        "git_graph.menu.merge_branch_into_current",
                        name = name.clone()
                    ),
                    GitGraphAction::Merge { rev: name.clone() },
                ));
                items.push(MenuItem::Separator);
                items.push(
                    confirm_item(
                        &weak,
                        tr!("git_graph.menu.delete_remote_branch"),
                        tr!("git_graph.dialog.delete_remote_title"),
                        tr!(
                            "git_graph.dialog.delete_remote_message",
                            name = name.clone()
                        ),
                        tr!("git_graph.menu.delete"),
                        GitGraphAction::DeleteRemoteBranch { name: name.clone() },
                    )
                    .icon("icons/trash.svg"),
                );
            }
            GitGraphRefKind::Tag => {
                items.push(
                    action_item(
                        &weak,
                        tr!("git_graph.menu.push_tag"),
                        GitGraphAction::PushTag { name: name.clone() },
                    )
                    .icon("icons/cloud-upload.svg"),
                );
                items.push(action_item(
                    &weak,
                    tr!(
                        "git_graph.menu.merge_branch_into_current",
                        name = name.clone()
                    ),
                    GitGraphAction::Merge { rev: name.clone() },
                ));
                items.push(
                    confirm_item(
                        &weak,
                        tr!("git_graph.menu.delete_tag"),
                        tr!("git_graph.dialog.delete_tag_title"),
                        tr!("git_graph.dialog.delete_tag_message", name = name.clone()),
                        tr!("git_graph.menu.delete"),
                        GitGraphAction::DeleteTag { name: name.clone() },
                    )
                    .icon("icons/trash.svg"),
                );
            }
            GitGraphRefKind::Head => {
                let weak = weak.clone();
                let start = hash.to_owned();
                let label = super::git_graph::short_hash(hash).to_owned();
                items.push(
                    MenuItem::new(tr!("git_graph.menu.create_branch"), move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.open_git_graph_dialog(
                                GitGraphDialogKind::CreateBranch {
                                    start: start.clone(),
                                    label: label.clone(),
                                },
                                window,
                                cx,
                            )
                        });
                    })
                    .icon("icons/git-branch.svg"),
                );
            }
        }
        items.push(MenuItem::Separator);
        let copy = name.clone();
        items.push(
            MenuItem::new(tr!("git_graph.menu.copy_name"), move |_, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.copy_git_graph_text(copy.clone(), tr!("git_graph.copied_name"), cx)
                });
            })
            .icon("icons/copy.svg"),
        );
        items
    }
}

/// A menu entry that runs an action straight away.
fn action_item(weak: &WeakEntity<Waku>, label: String, action: GitGraphAction) -> MenuItem {
    let weak = weak.clone();
    MenuItem::new(label, move |_, cx| {
        let _ = weak.update(cx, |this, cx| this.run_git_graph_action(action.clone(), cx));
    })
}

/// A menu entry that asks first, for actions that rewrite history or delete.
fn confirm_item(
    weak: &WeakEntity<Waku>,
    label: String,
    title: String,
    message: String,
    confirm: String,
    action: GitGraphAction,
) -> MenuItem {
    let weak = weak.clone();
    MenuItem::new(label, move |window, cx| {
        let _ = weak.update(cx, |this, cx| {
            this.open_git_graph_dialog(
                GitGraphDialogKind::Confirm {
                    title: title.clone(),
                    message: message.clone(),
                    confirm: confirm.clone(),
                    action: action.clone(),
                },
                window,
                cx,
            )
        });
    })
}

fn action_success_message(action: &GitGraphAction) -> String {
    match action {
        GitGraphAction::CheckoutCommit { .. }
        | GitGraphAction::CheckoutBranch { .. }
        | GitGraphAction::CheckoutRemoteBranch { .. } => tr!("git_graph.done.checked_out"),
        GitGraphAction::CreateBranch { name, .. } => {
            tr!("git_graph.done.branch_created", name = name.clone())
        }
        GitGraphAction::RenameBranch { to, .. } => {
            tr!("git_graph.done.branch_renamed", name = to.clone())
        }
        GitGraphAction::DeleteBranch { name, .. } | GitGraphAction::DeleteRemoteBranch { name } => {
            tr!("git_graph.done.branch_deleted", name = name.clone())
        }
        GitGraphAction::PushBranch { name } | GitGraphAction::PushTag { name } => {
            tr!("git_graph.done.pushed", name = name.clone())
        }
        GitGraphAction::CreateTag { name, .. } => {
            tr!("git_graph.done.tag_created", name = name.clone())
        }
        GitGraphAction::DeleteTag { name } => {
            tr!("git_graph.done.tag_deleted", name = name.clone())
        }
        GitGraphAction::Merge { .. } => tr!("git_graph.done.merged"),
        GitGraphAction::Rebase { .. } => tr!("git_graph.done.rebased"),
        GitGraphAction::CherryPick { .. } => tr!("git_graph.done.cherry_picked"),
        GitGraphAction::Revert { .. } => tr!("git_graph.done.reverted"),
        GitGraphAction::Reset { .. } => tr!("git_graph.done.reset"),
        GitGraphAction::Stash => tr!("git_graph.done.stashed"),
        GitGraphAction::DiscardUncommitted => tr!("git_graph.done.discarded"),
    }
}

fn compare_side(label: String, subject: String, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w_0()
        .text_size(sp(11.5))
        .child(
            div()
                .flex_none()
                .text_color(theme.text_tertiary)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(theme.text_secondary)
                .child(subject),
        )
}

fn diff_caption(label: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .w_full()
        .min_w_0()
        .min_h(px(20.0))
        .flex_none()
        .px(px(10.0))
        .py(px(2.0))
        .flex()
        .items_center()
        .font_family(md::render::MONO_FAMILY)
        .text_size(px(12.0))
        .bg(theme.overlay)
        .text_color(theme.text_tertiary)
        .whitespace_normal()
        .child(label.into())
}
