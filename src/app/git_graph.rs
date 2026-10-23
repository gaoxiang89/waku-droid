//! Right-panel Git Graph.
//!
//! The picture matches the Git Graph extension: one row per commit, colored
//! lanes, ref capsules, and an uncommitted row at the top. `git` runs on the
//! daemon. A frame only reads the query cache and the layout stored on the
//! snapshot.

use std::sync::Arc;

use gpui::{
    InteractiveElement, KeyBinding, PathBuilder, StatefulInteractiveElement, actions, hsla,
};

use super::*;

const GIT_GRAPH_CONTEXT: &str = "GitGraph";
const GIT_GRAPH_SEARCH_CONTEXT: &str = "GitGraph > TextInput";
const ROW_HEIGHT: f32 = 48.0;
const LANE_PITCH: f32 = 12.0;
const LANE_PAD: f32 = 10.0;
const PAGE: u32 = 400;
const MAX_LIMIT: u32 = 4_000;

const LANE_HUES: [f32; 8] = [211.0, 28.0, 152.0, 274.0, 48.0, 332.0, 188.0, 12.0];

actions!(waku_git_graph, [GitGraphTop, GitGraphBottom]);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNextEntry, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("up", SelectPreviousEntry, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("down", SelectNextEntry, Some(GIT_GRAPH_SEARCH_CONTEXT)),
        KeyBinding::new("up", SelectPreviousEntry, Some(GIT_GRAPH_SEARCH_CONTEXT)),
        KeyBinding::new("home", GitGraphTop, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("end", GitGraphBottom, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("home", GitGraphTop, Some(GIT_GRAPH_SEARCH_CONTEXT)),
        KeyBinding::new("end", GitGraphBottom, Some(GIT_GRAPH_SEARCH_CONTEXT)),
        KeyBinding::new("enter", ConfirmEntry, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("escape", DismissMenu, Some(GIT_GRAPH_CONTEXT)),
    ]);
}

enum GitGraphView {
    NoProject,
    Loading,
    NotARepository,
    Error(String),
    Ready(Arc<waku_client::git::GitGraphSnapshot>),
}

impl Waku {
    pub(super) fn clear_git_graph_cache(&mut self) {
        self.git_graph_snapshots.clear();
        self.git_commit_details.clear();
        self.git_graph_limits.clear();
        self.visible_git_graph = None;
        self.git_graph_cursor = None;
    }

    fn refresh_git_graph(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.selected_workspace_path() {
            let path = path.to_path_buf();
            self.git_graph_snapshots.invalidate(&path);
        }
        cx.notify();
    }

    fn load_more_git_graph(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return;
        };
        let limit = self.git_graph_limits.entry(path.clone()).or_insert(PAGE);
        if *limit >= MAX_LIMIT {
            return;
        }
        *limit = limit.saturating_add(PAGE).min(MAX_LIMIT);
        self.git_graph_snapshots.invalidate(&path);
        cx.notify();
    }

    fn git_graph_page_limit(&self, path: &Path) -> u32 {
        self.git_graph_limits.get(path).copied().unwrap_or(PAGE)
    }

    /// The graph for the selected workspace. A miss starts one daemon fetch.
    fn git_graph_view(&mut self, cx: &mut Context<Self>) -> GitGraphView {
        let Some(path) = self.selected_workspace_path().map(Path::to_path_buf) else {
            return GitGraphView::NoProject;
        };
        let limit = self.git_graph_page_limit(&path);
        let read = match self.git_graph_snapshots.read(&path) {
            Query::Ready(value) => Some(value),
            Query::Pending => None,
            Query::Missing(token) => {
                self.spawn_git_graph(path.clone(), limit, token, cx);
                None
            }
        };
        if let Some(value) = read {
            return match value.as_ref() {
                Ok(Some(snapshot)) => {
                    self.visible_git_graph = Some((path, Arc::clone(snapshot)));
                    GitGraphView::Ready(Arc::clone(snapshot))
                }
                Ok(None) => GitGraphView::NotARepository,
                Err(error) => self.stale_or_error(&path, error),
            };
        }
        self.stale_or_loading(&path)
    }

    fn stale_or_error(&self, path: &Path, error: &str) -> GitGraphView {
        match &self.visible_git_graph {
            Some((cached, snapshot)) if cached == path => GitGraphView::Ready(Arc::clone(snapshot)),
            _ => GitGraphView::Error(error.to_owned()),
        }
    }

    fn stale_or_loading(&self, path: &Path) -> GitGraphView {
        match &self.visible_git_graph {
            Some((cached, snapshot)) if cached == path => GitGraphView::Ready(Arc::clone(snapshot)),
            _ => GitGraphView::Loading,
        }
    }

    fn spawn_git_graph(
        &self,
        path: PathBuf,
        limit: u32,
        token: crate::query::FetchToken<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        match workspace.request(waku_client::WorkspaceOperation::InspectGitGraph {
                            cwd: path,
                            limit,
                        }) {
                            Ok(waku_client::WorkspaceResult::GitGraph { snapshot }) => {
                                Ok(snapshot.map(Arc::new))
                            }
                            Ok(_) => Err(
                                "the daemon returned an invalid git graph response".to_owned()
                            ),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                if !waku.git_graph_snapshots.fulfill(token, result.clone()) {
                    return;
                }
                if let Ok(Some(snapshot)) = &result {
                    waku.visible_git_graph = Some((path, Arc::clone(snapshot)));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn git_commit_detail(
        &mut self,
        hash: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Result<waku_client::git::GitCommitDetail, String>>> {
        let path = self.selected_workspace_path()?.to_path_buf();
        let key = (path.clone(), hash.to_owned());
        match self.git_commit_details.read(&key) {
            Query::Ready(value) => Some(value),
            Query::Pending => None,
            Query::Missing(token) => {
                self.spawn_git_commit_detail(path, hash.to_owned(), token, cx);
                None
            }
        }
    }

    fn spawn_git_commit_detail(
        &self,
        path: PathBuf,
        hash: String,
        token: crate::query::FetchToken<(PathBuf, String)>,
        cx: &mut Context<Self>,
    ) {
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    let hash = hash.clone();
                    async move {
                        match workspace
                            .request(waku_client::WorkspaceOperation::InspectGitCommit {
                                cwd: path,
                                hash,
                            }) {
                            Ok(waku_client::WorkspaceResult::GitCommit { detail }) => Ok(detail),
                            Ok(_) => Err(
                                "the daemon returned an invalid commit response".to_owned()
                            ),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                if waku.git_commit_details.fulfill(token, result) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn step_git_graph(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(snapshot) = self
            .visible_git_graph
            .as_ref()
            .filter(|(path, _)| {
                self.selected_workspace_path()
                    .is_some_and(|selected| selected == path)
            })
            .map(|(_, snapshot)| Arc::clone(snapshot))
        else {
            return;
        };
        let count = git_graph_row_count(&snapshot);
        if count == 0 {
            return;
        }
        let next = match self.git_graph_cursor {
            None if delta < 0 => count - 1,
            None => 0,
            Some(current) => {
                let next = current as isize + delta;
                if next < 0 {
                    0
                } else {
                    (next as usize).min(count - 1)
                }
            }
        };
        self.focus_git_graph_row(next, cx);
    }

    fn focus_git_graph_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.git_graph_cursor = Some(index);
        self.git_graph_list_state.scroll_to_reveal_item(index);
        cx.notify();
    }

    fn git_graph_query(&self, cx: &App) -> String {
        self.git_graph_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase()
    }

    pub(super) fn render_git_graph(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let view = self.git_graph_view(cx);
        let mut column = div()
            .id("git-graph")
            .key_context(GIT_GRAPH_CONTEXT)
            .tab_group()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &SelectNextEntry, _, cx| {
                this.step_git_graph(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousEntry, _, cx| {
                this.step_git_graph(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &GitGraphTop, _, cx| {
                this.focus_git_graph_row(0, cx);
            }))
            .on_action(cx.listener(|this, _: &GitGraphBottom, _, cx| {
                let Some(count) = this.visible_git_graph.as_ref().map(|(_, snapshot)| {
                    git_graph_row_count(snapshot)
                }) else {
                    return;
                };
                if count > 0 {
                    this.focus_git_graph_row(count - 1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ConfirmEntry, _, cx| {
                if this.git_graph_cursor.is_none() {
                    this.focus_git_graph_row(0, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DismissMenu, _, cx| {
                this.git_graph_cursor = None;
                cx.notify();
            }))
            .child(self.render_git_graph_toolbar(&theme, cx));

        column = match view {
            GitGraphView::NoProject => column.child(self.render_right_panel_empty_message(
                tr!("git_graph.no_project"),
                tr!("git_graph.no_project_description"),
                cx,
            )),
            GitGraphView::Loading => column.child(self.render_right_panel_empty_message(
                tr!("git_graph.loading"),
                tr!("git_graph.loading_description"),
                cx,
            )),
            GitGraphView::NotARepository => column.child(self.render_right_panel_empty_message(
                tr!("git_graph.not_a_repository"),
                tr!("git_graph.not_a_repository_description"),
                cx,
            )),
            GitGraphView::Error(error) => column.child(self.render_right_panel_empty_message(
                tr!("git_graph.error"),
                error,
                cx,
            )),
            GitGraphView::Ready(snapshot) => {
                if snapshot.commits.is_empty() && snapshot.uncommitted.is_empty() {
                    column.child(self.render_right_panel_empty_message(
                        tr!("git_graph.empty"),
                        tr!("git_graph.empty_description"),
                        cx,
                    ))
                } else {
                    column.child(self.render_git_graph_ready(snapshot, &theme, cx))
                }
            }
        };
        column.into_any_element()
    }

    fn render_git_graph_toolbar(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let refresh_focus = self.git_graph_refresh_focus.clone();
        div()
            .flex_none()
            .h(px(44.0))
            .px(px(10.0))
            .gap(px(6.0))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                TextField::new("git-graph-search", self.git_graph_search.clone())
                    .icon("icons/search.svg", 13.0)
                    .flex_1(),
            )
            .child(
                icon_button("git-graph-refresh", "icons/rotate-cw.svg", *theme)
                    .track_focus(&refresh_focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .tooltip(Tooltip::text(tr!("git_graph.refresh")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_git_graph(cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.refresh_git_graph(cx);
                        }
                    })),
            )
    }

    fn render_git_graph_ready(
        &mut self,
        snapshot: Arc<waku_client::git::GitGraphSnapshot>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let rows = git_graph_row_count(&snapshot);
        if self.git_graph_cursor.is_some_and(|cursor| cursor >= rows) {
            self.git_graph_cursor = None;
        }
        if self.git_graph_list_rows != rows {
            self.git_graph_list_rows = rows;
            self.git_graph_list_state
                .reset_with_uniform_height(rows, px(ROW_HEIGHT));
        }
        let width = graph_width(&snapshot);
        let listed = snapshot.clone();
        let entity = cx.entity().downgrade();
        let mut body = div().flex_1().min_h_0().flex().flex_col().child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .track_focus(&self.git_graph_focus)
                .tab_index(0)
                .tab_stop(true)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .child(
                    list(
                        self.git_graph_list_state.clone(),
                        move |index, _window, cx| {
                            let listed = listed.clone();
                            entity
                                .upgrade()
                                .map(|entity| {
                                    entity.update(cx, |this, cx| {
                                        this.git_graph_row(index, &listed, width, cx)
                                    })
                                })
                                .unwrap_or_else(|| div().into_any_element())
                        },
                    )
                    .size_full(),
                )
                .child(scrollbar::vertical(
                    &self.git_graph_list_state,
                    &self.git_graph_scrollbar,
                )),
        );
        if let Some(cursor) = self.git_graph_cursor {
            body = body.child(self.render_git_graph_detail(cursor, &snapshot, theme, cx));
        }
        if snapshot.truncated && self.git_graph_page_limit_for_snapshot(&snapshot) < MAX_LIMIT {
            body = body.child(self.render_git_graph_load_more(theme, cx));
        }
        body
    }

    fn git_graph_page_limit_for_snapshot(
        &self,
        _snapshot: &waku_client::git::GitGraphSnapshot,
    ) -> u32 {
        self.selected_workspace_path()
            .map(|path| self.git_graph_page_limit(path))
            .unwrap_or(PAGE)
    }

    fn git_graph_row(
        &self,
        index: usize,
        snapshot: &waku_client::git::GitGraphSnapshot,
        graph_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let query = self.git_graph_query(cx);
        let selected = self.git_graph_cursor == Some(index);
        let matched = git_graph_row_matches(snapshot, index, &query);
        let uncommitted = !snapshot.uncommitted.is_empty();
        let commit_index = index.saturating_sub(usize::from(uncommitted));
        let row = if uncommitted && index == 0 {
            None
        } else {
            snapshot.rows.get(commit_index).cloned()
        };
        let head_lane = snapshot
            .rows
            .first()
            .map(|row| row.node_lane)
            .unwrap_or(0);
        let label = if uncommitted && index == 0 {
            tr!("git_graph.uncommitted")
        } else {
            snapshot
                .commits
                .get(commit_index)
                .map(|commit| commit.subject.clone())
                .unwrap_or_default()
        };
        let secondary = snapshot.commits.get(commit_index).map(|commit| {
            let age = unix_time() as i64;
            let seconds = age.saturating_sub(commit.authored_at).max(0) as u64;
            format!(
                "{} · {} · {}",
                commit.author,
                super::sidebar::format_time_ago(seconds),
                short_hash(&commit.hash)
            )
        });
        let refs = if uncommitted && index == 0 {
            Vec::new()
        } else {
            snapshot
                .commits
                .get(commit_index)
                .map(|commit| commit.refs.clone())
                .unwrap_or_default()
        };

        div()
            .id(SharedString::from(format!("git-graph-row-{index}")))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .px(px(8.0))
            .gap(px(8.0))
            .cursor_default()
            .when(!matched, |element| element.opacity(0.38))
            .when(selected, |element| element.bg(theme.overlay_strong))
            .when(!selected, |element| {
                element.hover(|element| element.bg(theme.overlay))
            })
            .child(git_graph_lanes(
                row,
                uncommitted && index == 0,
                uncommitted && index == 1,
                head_lane,
                graph_width,
            ))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(1.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_shrink_1()
                                    .text_size(sp(12.5))
                                    .line_height(sp(16.0))
                                    .font_weight(if selected {
                                        FontWeight::MEDIUM
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(theme.text)
                                    .truncate()
                                    .child(label),
                            )
                            .children(refs.iter().map(|git_ref| git_ref_chip(git_ref, &theme))),
                    )
                    .when_some(secondary, |element, secondary| {
                        element.child(
                            div()
                                .text_size(sp(11.5))
                                .line_height(sp(14.0))
                                .text_color(theme.text_tertiary)
                                .truncate()
                                .child(secondary),
                        )
                    }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus_git_graph_row(index, cx);
                window.focus(&this.git_graph_focus, cx);
            }))
            .into_any_element()
    }

    fn render_git_graph_detail(
        &mut self,
        index: usize,
        snapshot: &waku_client::git::GitGraphSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let uncommitted = !snapshot.uncommitted.is_empty();
        if uncommitted && index == 0 {
            return self.render_git_graph_uncommitted_detail(snapshot, theme);
        }
        let Some(commit) = snapshot
            .commits
            .get(index.saturating_sub(usize::from(uncommitted)))
        else {
            return div().id("git-graph-detail-empty");
        };
        let hash = commit.hash.clone();
        let detail = self.git_commit_detail(&hash, cx);
        let authored = DateTime::<Utc>::from_timestamp(commit.authored_at, 0)
            .map(|time| {
                time.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        let mut body = vertical_scroll(
            div()
                .flex_none()
                .max_h(px(240.0))
                .border_t_1()
                .border_color(theme.border),
        )
        .track_scroll(&self.git_graph_detail_scroll)
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
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
            .child(self.render_git_graph_hash(&hash, theme, cx));
        if !commit.parents.is_empty() {
            body = body.child(
                div()
                    .text_size(sp(11.5))
                    .text_color(theme.text_secondary)
                    .child(format!(
                        "{} {}",
                        tr!("git_graph.parents"),
                        short_hashes(&commit.parents)
                    )),
            );
        }
        match detail.as_deref() {
            Some(Ok(detail)) => {
                let extra = message_body(&commit.subject, &detail.body);
                if let Some(extra) = extra {
                    body = body.child(
                        div()
                            .text_size(sp(12.0))
                            .line_height(sp(17.0))
                            .text_color(theme.text_secondary)
                            .whitespace_normal()
                            .child(extra.to_owned()),
                    );
                }
                body = body.child(git_graph_files(&detail.files, theme));
            }
            Some(Err(error)) => {
                body = body.child(
                    div()
                        .text_size(sp(12.0))
                        .text_color(theme.danger)
                        .child(error.clone()),
                );
            }
            None => {
                body = body.child(
                    div()
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(tr!("git_graph.loading")),
                );
            }
        }
        body
    }

    fn render_git_graph_uncommitted_detail(
        &self,
        snapshot: &waku_client::git::GitGraphSnapshot,
        theme: &Theme,
    ) -> Stateful<Div> {
        vertical_scroll(
            div()
                .flex_none()
                .max_h(px(240.0))
                .border_t_1()
                .border_color(theme.border),
        )
        .track_scroll(&self.git_graph_detail_scroll)
            .px(px(12.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(sp(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(tr!("git_graph.uncommitted")),
            )
            .child(git_graph_files(&snapshot.uncommitted, theme))
    }

    fn render_git_graph_hash(
        &self,
        hash: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let copied = hash.to_owned();
        div()
            .id("git-graph-copy-hash")
            .track_focus(&self.git_graph_copy_focus)
            .tab_index(0)
            .tab_stop(true)
            .h(px(24.0))
            .px(px(6.0))
            .w_full()
            .rounded(px(5.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .hover(|element| element.bg(theme.overlay))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .tooltip(Tooltip::text(tr!("git_graph.copy_hash")))
            .child(icon("icons/copy.svg", 12.0, theme.text_tertiary))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(sp(11.5))
                    .text_color(theme.text_secondary)
                    .truncate()
                    .child(hash.to_owned()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.copy_git_hash(copied.clone(), cx);
            }))
            .on_key_down(cx.listener({
                let copied = hash.to_owned();
                move |this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.copy_git_hash(copied.clone(), cx);
                    }
                }
            }))
    }

    fn copy_git_hash(&mut self, hash: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(hash));
        self.show_success_toast(tr!("git_graph.copied"));
        cx.notify();
    }

    fn render_git_graph_load_more(&self, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("git-graph-load-more")
            .track_focus(&self.git_graph_load_more_focus)
            .tab_index(0)
            .tab_stop(true)
            .flex_none()
            .h(px(32.0))
            .mx(px(8.0))
            .my(px(6.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(sp(12.5))
            .text_color(theme.text_secondary)
            .cursor_default()
            .hover(|element| element.bg(theme.overlay))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(tr!("git_graph.load_more"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.load_more_git_graph(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.load_more_git_graph(cx);
                }
            }))
    }
}

fn vertical_scroll(element: Div) -> Stateful<Div> {
    // `overflow_y_scroll` and `track_scroll` live on `StatefulInteractiveElement`,
    // so the pane needs an element id before it can scroll.
    element.id("git-graph-detail-scroll").overflow_y_scroll()
}

fn git_graph_row_count(snapshot: &waku_client::git::GitGraphSnapshot) -> usize {
    snapshot.commits.len() + usize::from(!snapshot.uncommitted.is_empty())
}

fn git_graph_row_matches(
    snapshot: &waku_client::git::GitGraphSnapshot,
    index: usize,
    query: &str,
) -> bool {
    if query.is_empty() {
        return true;
    }
    let uncommitted = !snapshot.uncommitted.is_empty();
    if uncommitted && index == 0 {
        return tr!("git_graph.uncommitted").to_lowercase().contains(query)
            || snapshot
                .uncommitted
                .iter()
                .any(|file| file.path.to_lowercase().contains(query));
    }
    let Some(commit) = snapshot
        .commits
        .get(index.saturating_sub(usize::from(uncommitted)))
    else {
        return false;
    };
    commit.subject.to_lowercase().contains(query)
        || commit.author.to_lowercase().contains(query)
        || commit.hash.to_lowercase().contains(query)
        || commit
            .refs
            .iter()
            .any(|git_ref| git_ref.name.to_lowercase().contains(query))
}

fn git_ref_chip(git_ref: &waku_client::git::GitGraphRef, theme: &Theme) -> Div {
    let color = match git_ref.kind {
        waku_client::git::GitGraphRefKind::Tag => theme.warning,
        waku_client::git::GitGraphRefKind::Remote => theme.text_tertiary,
        waku_client::git::GitGraphRefKind::Head | waku_client::git::GitGraphRefKind::Branch
            if git_ref.current =>
        {
            theme.accent
        }
        _ => theme.text_secondary,
    };
    div()
        .flex_none()
        .h(px(16.0))
        .px(px(5.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(color)
        .flex()
        .items_center()
        .gap(px(4.0))
        .text_size(sp(10.0))
        .line_height(sp(12.0))
        .text_color(color)
        .child(git_ref.name.clone())
        .when(git_ref.current, |element| {
            element.child(tr!("git_graph.head"))
        })
}

fn git_graph_files(files: &[waku_client::git::GitGraphFile], theme: &Theme) -> Div {
    if files.is_empty() {
        return div()
            .text_size(sp(12.0))
            .text_color(theme.text_tertiary)
            .child(tr!("git_graph.no_files"));
    }
    let mut list = div().flex().flex_col().gap(px(2.0));
    for file in files {
        let color = match file.status {
            waku_client::git::GitGraphFileStatus::Added => theme.success,
            waku_client::git::GitGraphFileStatus::Deleted => theme.danger,
            waku_client::git::GitGraphFileStatus::Modified
            | waku_client::git::GitGraphFileStatus::TypeChanged => theme.warning,
            _ => theme.text_secondary,
        };
        let path = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path),
            None => file.path.clone(),
        };
        list = list.child(
            div()
                .h(px(20.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .min_w_0()
                .child(
                    div()
                        .w(px(14.0))
                        .flex_none()
                        .text_size(sp(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(color)
                        .child(status_letter(file.status)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_size(sp(12.0))
                        .text_color(theme.text)
                        .truncate()
                        .child(path),
                ),
        );
    }
    list
}

fn status_letter(status: waku_client::git::GitGraphFileStatus) -> &'static str {
    match status {
        waku_client::git::GitGraphFileStatus::Added => "A",
        waku_client::git::GitGraphFileStatus::Modified => "M",
        waku_client::git::GitGraphFileStatus::Deleted => "D",
        waku_client::git::GitGraphFileStatus::Renamed => "R",
        waku_client::git::GitGraphFileStatus::Copied => "C",
        waku_client::git::GitGraphFileStatus::Untracked => "?",
        waku_client::git::GitGraphFileStatus::TypeChanged => "T",
    }
}

fn message_body<'a>(subject: &str, body: &'a str) -> Option<&'a str> {
    let body = body.trim();
    let rest = body.strip_prefix(subject).unwrap_or(body).trim();
    if rest.is_empty() { None } else { Some(rest) }
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(7)]
}

fn short_hashes(parents: &[String]) -> String {
    parents
        .iter()
        .map(|hash| short_hash(hash))
        .collect::<Vec<_>>()
        .join(" ")
}

fn graph_width(snapshot: &waku_client::git::GitGraphSnapshot) -> f32 {
    let lanes = snapshot
        .rows
        .iter()
        .map(|row| row.lanes.len())
        .max()
        .unwrap_or(1)
        .max(1);
    LANE_PAD + lanes as f32 * LANE_PITCH
}

fn git_graph_lanes(
    row: Option<waku_client::git::GitGraphRow>,
    uncommitted: bool,
    connect_above: bool,
    head_lane: u16,
    width: f32,
) -> impl IntoElement {
    canvas(
        {
            let row = row.clone();
            move |_, _, _| (row.clone(), uncommitted, connect_above, head_lane)
        },
        move |bounds, (row, uncommitted, connect_above, head_lane), window, cx| {
            let theme = Theme::current(cx);
            if uncommitted {
                paint_uncommitted_marker(bounds, head_lane, theme.text_ghost, window);
                return;
            }
            let Some(row) = row else {
                return;
            };
            paint_commit_row(
                bounds,
                &row,
                connect_above,
                theme.text_ghost,
                &theme,
                window,
            );
        },
    )
    .w(px(width))
    .h(px(ROW_HEIGHT))
    .flex_none()
}

fn lane_x(origin_x: Pixels, lane: u16) -> Pixels {
    origin_x + px(LANE_PAD) + px(lane as f32 * LANE_PITCH) + px(LANE_PITCH / 2.0)
}

fn lane_hsla(theme: &Theme, color: u8) -> Hsla {
    let hue = LANE_HUES[(color as usize) % LANE_HUES.len()] / 360.0;
    let lightness = if theme.is_dark { 0.68 } else { 0.42 };
    hsla(hue, 0.58, lightness, 1.0)
}

fn paint_commit_row(
    bounds: gpui::Bounds<Pixels>,
    row: &waku_client::git::GitGraphRow,
    connect_above: bool,
    ghost: Hsla,
    theme: &Theme,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let bottom = top + bounds.size.height;
    let mid = top + bounds.size.height / 2.0;
    let radius = px(4.0);
    for (index, lane) in row.lanes.iter().enumerate() {
        if lane.node {
            continue;
        }
        let x = lane_x(bounds.origin.x, index as u16);
        let y0 = if lane.up { top } else { mid };
        let y1 = if lane.down { bottom } else { mid };
        if y1 > y0 {
            stroke_line(window, x, y0, x, y1, lane_hsla(theme, lane.color));
        }
    }
    for edge in &row.edges {
        let x0 = lane_x(bounds.origin.x, edge.from);
        let x1 = lane_x(bounds.origin.x, edge.to);
        let color = lane_hsla(theme, edge.color);
        let mut path = PathBuilder::stroke(px(1.5));
        path.move_to(point(x0, mid));
        path.cubic_bezier_to(
            point(x1, bottom),
            point(x0, mid + (bottom - mid) * 0.65),
            point(x1, mid + (bottom - mid) * 0.35),
        );
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
    for (index, lane) in row.lanes.iter().enumerate() {
        if !lane.node {
            continue;
        }
        let x = lane_x(bounds.origin.x, index as u16);
        let color = lane_hsla(theme, lane.color);
        if lane.up {
            stroke_line(window, x, top, x, mid - radius, color);
        } else if connect_above {
            // The uncommitted marker sits in the row above. Continue its
            // dashed stem down to this commit's dot.
            paint_dashed_vertical(window, x, top, mid - radius, ghost);
        }
        if lane.down {
            stroke_line(window, x, mid + radius, x, bottom, color);
        }
        fill_circle(window, point(x, mid), radius, color);
    }
}

fn paint_uncommitted_marker(
    bounds: gpui::Bounds<Pixels>,
    lane: u16,
    color: Hsla,
    window: &mut Window,
) {
    let x = lane_x(bounds.origin.x, lane);
    let mid = bounds.origin.y + bounds.size.height / 2.0;
    let bottom = bounds.origin.y + bounds.size.height;
    paint_dashed_vertical(window, x, mid + px(6.0), bottom, color);
    let mut ring = PathBuilder::stroke(px(1.25));
    stroke_circle(&mut ring, point(x, mid), px(3.5));
    if let Ok(path) = ring.build() {
        window.paint_path(path, color);
    }
}

fn paint_dashed_vertical(
    window: &mut Window,
    x: Pixels,
    y0: Pixels,
    y1: Pixels,
    color: Hsla,
) {
    let mut line = PathBuilder::stroke(px(1.25)).dash_array(&[px(2.0), px(3.0)]);
    line.move_to(point(x, y0));
    line.line_to(point(x, y1));
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
}

fn stroke_line(window: &mut Window, x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels, color: Hsla) {
    let mut path = PathBuilder::stroke(px(1.5));
    path.move_to(point(x0, y0));
    path.line_to(point(x1, y1));
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn fill_circle(window: &mut Window, center: gpui::Point<Pixels>, radius: Pixels, color: Hsla) {
    let mut path = PathBuilder::fill();
    stroke_circle(&mut path, center, radius);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn stroke_circle(path: &mut PathBuilder, center: gpui::Point<Pixels>, radius: Pixels) {
    path.move_to(point(center.x + radius, center.y));
    path.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x - radius, center.y),
    );
    path.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x + radius, center.y),
    );
    path.close();
}
