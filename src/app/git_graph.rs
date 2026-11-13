//! Right-panel Git Graph.
//!
//! One row per commit, colored metro lanes, leading ref capsules, and an
//! uncommitted row at the top. `git` runs on the daemon; frames only read the
//! query cache and the layout stored on the snapshot.

use std::sync::Arc;

use gpui::{
    ClickEvent, InteractiveElement, KeyBinding, PathBuilder, Rgba, StatefulInteractiveElement,
    actions, hsla, linear_color_stop, linear_gradient,
};

use super::*;

const GIT_GRAPH_CONTEXT: &str = "GitGraph";
const GIT_GRAPH_SEARCH_CONTEXT: &str = "GitGraph > TextInput";
const ROW_HEIGHT: f32 = 50.0;
const ROW_LEFT_PAD: f32 = 8.0;
const HEADER_HEIGHT: f32 = 26.0;
pub(super) const GIT_GRAPH_MIN_COLUMN_WIDTH: f32 = 48.0;
pub(super) const GIT_GRAPH_MIN_COMMIT_WIDTH: f32 = 200.0;
const PAGE: u32 = 400;
const MAX_LIMIT: u32 = 4_000;

/// Curated 8-slot metro palette: Sky Blue, Amber-Orange, Emerald, Violet,
/// Cyan, Rose, Gold, Coral.
const LANE_HUES: [f32; 8] = [212.0, 28.0, 152.0, 272.0, 188.0, 334.0, 46.0, 8.0];

actions!(
    waku_git_graph,
    [GitGraphTop, GitGraphBottom, GitGraphNextMatch, GitGraphPrevMatch]
);

pub(super) fn init(cx: &mut App) {
    super::git_graph_actions::init(cx);
    // Home/End stay with the search field's caret; they only jump rows when
    // the list itself has focus.
    cx.bind_keys([
        KeyBinding::new("down", SelectNextEntry, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("up", SelectPreviousEntry, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("down", SelectNextEntry, Some(GIT_GRAPH_SEARCH_CONTEXT)),
        KeyBinding::new("up", SelectPreviousEntry, Some(GIT_GRAPH_SEARCH_CONTEXT)),
        KeyBinding::new("enter", GitGraphNextMatch, Some(GIT_GRAPH_SEARCH_CONTEXT)),
        KeyBinding::new(
            "shift-enter",
            GitGraphPrevMatch,
            Some(GIT_GRAPH_SEARCH_CONTEXT),
        ),
        KeyBinding::new("home", GitGraphTop, Some(GIT_GRAPH_CONTEXT)),
        KeyBinding::new("end", GitGraphBottom, Some(GIT_GRAPH_CONTEXT)),
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

/// What a selected row points at, so a refresh that adds or drops rows keeps
/// the same commit selected instead of the same index.
enum GitGraphRowKey {
    Uncommitted,
    Commit(String),
}

/// Per-row search result for one snapshot and query, rebuilt only when either
/// changes so row builders never lowercase strings on a frame.
pub(super) struct GitGraphMatches {
    snapshot: Arc<waku_client::git::GitGraphSnapshot>,
    query: String,
    matched: Rc<Vec<bool>>,
    match_indices: Rc<Vec<usize>>,
}

#[derive(Clone)]
pub(super) struct RefJumpItem {
    display_ref: DisplayGitRef,
    row_index: usize,
    lane_color: Option<u8>,
    short_hash: String,
}

/// Cached grouped refs for the toolbar "Jump to branch or tag" popover,
/// rebuilt only when the snapshot pointer changes.
pub(super) struct GitGraphRefsCache {
    snapshot: Arc<waku_client::git::GitGraphSnapshot>,
    branches: Rc<Vec<RefJumpItem>>,
    tags: Rc<Vec<RefJumpItem>>,
    remotes: Rc<Vec<RefJumpItem>>,
}

/// Adaptive lane geometry computed from the current Git Graph panel width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct GraphLayout {
    pub(super) column_width: f32,
    pub(super) canvas_width: f32,
    pub(super) pad_x: f32,
    pub(super) lane_pitch: f32,
    pub(super) node_radius: f32,
    pub(super) stroke_width: f32,
}

/// A local branch paired with any remote tracking refs on the same commit, or
/// a standalone ref (tag, detached HEAD, untracked remote).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DisplayGitRef {
    pub(super) primary: waku_client::git::GitGraphRef,
    pub(super) synced_remotes: Vec<waku_client::git::GitGraphRef>,
}

/// Color tokens for a ref capsule.
#[derive(Clone, Copy)]
pub(super) struct RefChipStyle {
    pub(super) bg: Hsla,
    pub(super) border: Hsla,
    pub(super) fg: Hsla,
    pub(super) icon: Hsla,
    pub(super) badge_bg: Hsla,
}

/// Everything a row builder needs, resolved once per frame.
#[derive(Clone)]
struct GitGraphRowContext {
    snapshot: Arc<waku_client::git::GitGraphSnapshot>,
    matches: Option<Rc<Vec<bool>>>,
    cursor: Option<usize>,
    /// The selected commit's hash, offered as the other side of "compare".
    cursor_hash: Option<String>,
    compare: Option<String>,
    head_lane: u16,
    head_on_top: bool,
    panel_width: f32,
    layout: GraphLayout,
    now: i64,
}

impl Waku {
    fn refresh_git_graph(&mut self, cx: &mut Context<Self>) {
        if let Some(scope) = self.selected_panel_scope() {
            self.git_graph_snapshots.invalidate(&scope);
        }
        cx.notify();
    }

    fn load_more_git_graph(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.selected_panel_scope() else {
            return;
        };
        let limit = self.git_graph_limits.entry(scope.clone()).or_insert(PAGE);
        if *limit >= MAX_LIMIT {
            return;
        }
        *limit = limit.saturating_add(PAGE).min(MAX_LIMIT);
        self.git_graph_snapshots.invalidate(&scope);
        cx.notify();
    }

    fn git_graph_page_limit(&self, scope: &PanelScope) -> u32 {
        self.git_graph_limits.get(scope).copied().unwrap_or(PAGE)
    }

    fn git_graph_cursor_in(&self, scope: &PanelScope) -> Option<usize> {
        self.git_graph_cursor
            .as_ref()
            .filter(|(cursor_scope, _)| cursor_scope == scope)
            .map(|(_, index)| *index)
    }

    fn selected_git_graph_cursor(&self) -> Option<usize> {
        self.git_graph_cursor_in(&self.selected_panel_scope()?)
    }

    pub(super) fn current_git_graph_column_width(&self, panel_width: f32) -> f32 {
        let lane_count = self
            .selected_panel_scope()
            .and_then(|scope| {
                self.visible_git_graph
                    .as_ref()
                    .filter(|(cached, _)| cached == &scope)
                    .map(|(_, snapshot)| snapshot.lane_count)
            })
            .unwrap_or(1);
        compute_graph_layout(panel_width, lane_count, self.git_graph_column_ratio).column_width
    }

    /// Carry the selection from the snapshot on screen to one that just
    /// arrived for the same workspace.
    fn remap_git_graph_cursor(
        &mut self,
        scope: &PanelScope,
        next: &waku_client::git::GitGraphSnapshot,
    ) {
        let Some(index) = self.git_graph_cursor_in(scope) else {
            return;
        };
        let previous = match &self.visible_git_graph {
            Some((visible, snapshot)) if visible == scope => Arc::clone(snapshot),
            _ => {
                self.git_graph_cursor = None;
                return;
            }
        };
        self.git_graph_cursor = git_graph_row_key(&previous, index)
            .and_then(|key| git_graph_row_index(next, &key))
            .map(|index| (scope.clone(), index));
    }

    /// The graph for the selected workspace. A miss starts one daemon fetch.
    fn git_graph_view(&mut self, cx: &mut Context<Self>) -> GitGraphView {
        let Some(scope) = self.selected_panel_scope() else {
            return GitGraphView::NoProject;
        };
        let limit = self.git_graph_page_limit(&scope);
        let read = match self.git_graph_snapshots.read(&scope) {
            Query::Ready(value) => Some(value),
            Query::Pending => None,
            Query::Missing(token) => {
                self.spawn_git_graph(scope.clone(), limit, token, cx);
                None
            }
        };
        if let Some(value) = read {
            return match value.as_ref() {
                Ok(Some(snapshot)) => {
                    self.visible_git_graph = Some((scope, Arc::clone(snapshot)));
                    GitGraphView::Ready(Arc::clone(snapshot))
                }
                Ok(None) => GitGraphView::NotARepository,
                Err(error) => self.stale_or_error(&scope, error),
            };
        }
        self.stale_or_loading(&scope)
    }

    fn stale_or_error(&self, scope: &PanelScope, error: &str) -> GitGraphView {
        match &self.visible_git_graph {
            Some((cached, snapshot)) if cached == scope => {
                GitGraphView::Ready(Arc::clone(snapshot))
            }
            _ => GitGraphView::Error(error.to_owned()),
        }
    }

    fn stale_or_loading(&self, scope: &PanelScope) -> GitGraphView {
        match &self.visible_git_graph {
            Some((cached, snapshot)) if cached == scope => {
                GitGraphView::Ready(Arc::clone(snapshot))
            }
            _ => GitGraphView::Loading,
        }
    }

    fn spawn_git_graph(
        &self,
        scope: PanelScope,
        limit: u32,
        token: crate::query::FetchToken<PanelScope>,
        cx: &mut Context<Self>,
    ) {
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = scope.workspace.clone();
                    async move {
                        match workspace.request(waku_client::WorkspaceOperation::InspectGitGraph {
                            cwd: path,
                            limit,
                        }) {
                            Ok(waku_client::WorkspaceResult::GitGraph { snapshot }) => {
                                Ok(snapshot.map(Arc::new))
                            }
                            Ok(_) => {
                                Err("the daemon returned an invalid git graph response".to_owned())
                            }
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
                    waku.remap_git_graph_cursor(&scope, snapshot);
                    waku.invalidate_git_graph_uncommitted_diffs(&scope);
                    waku.visible_git_graph = Some((scope, Arc::clone(snapshot)));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn git_commit_detail(
        &mut self,
        hash: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Result<waku_client::git::GitCommitDetail, String>>> {
        let scope = self.selected_panel_scope()?;
        let key = (scope.clone(), hash.to_owned());
        match self.git_commit_details.read(&key) {
            Query::Ready(value) => Some(value),
            Query::Pending => None,
            Query::Missing(token) => {
                self.spawn_git_commit_detail(scope, hash.to_owned(), token, cx);
                None
            }
        }
    }

    fn spawn_git_commit_detail(
        &self,
        scope: PanelScope,
        hash: String,
        token: crate::query::FetchToken<(PanelScope, String)>,
        cx: &mut Context<Self>,
    ) {
        let workspace = waku_client::WorkspaceClient::for_daemon(self.selected_daemon().clone());
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = scope.workspace.clone();
                    let hash = hash.clone();
                    async move {
                        match workspace.request(waku_client::WorkspaceOperation::InspectGitCommit {
                            cwd: path,
                            hash,
                        }) {
                            Ok(waku_client::WorkspaceResult::GitCommit { detail }) => Ok(detail),
                            Ok(_) => {
                                Err("the daemon returned an invalid commit response".to_owned())
                            }
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
        let Some(count) = self.selected_git_graph_row_count() else {
            return;
        };
        if count == 0 {
            return;
        }
        let next = match self.selected_git_graph_cursor() {
            None if delta < 0 => count - 1,
            None => 0,
            Some(current) => current.saturating_add_signed(delta).min(count - 1),
        };
        self.focus_git_graph_row(next, cx);
    }

    fn selected_git_graph_row_count(&self) -> Option<usize> {
        let selected = self.selected_panel_scope()?;
        self.visible_git_graph
            .as_ref()
            .filter(|(scope, _)| scope == &selected)
            .map(|(_, snapshot)| git_graph_row_count(snapshot))
    }

    pub(super) fn focus_git_graph_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(scope) = self.selected_panel_scope() else {
            return;
        };
        if self.git_graph_cursor_in(&scope) != Some(index) {
            self.git_graph_detail_scroll
                .set_offset(point(px(0.0), px(0.0)));
        }
        self.git_graph_cursor = Some((scope, index));
        self.git_graph_list_state.scroll_to_reveal_item(index);
        cx.notify();
    }

    pub(super) fn open_git_graph_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.focus_git_graph_row(index, cx);
        let Some(scope) = self.selected_panel_scope() else {
            return;
        };
        if let Some((_, snapshot)) = self
            .visible_git_graph
            .as_ref()
            .filter(|(shown, _)| shown == &scope)
            && let Some(GitGraphRowKey::Commit(hash)) = git_graph_row_key(snapshot, index)
            && let Some(commit) = snapshot.commits.iter().find(|commit| commit.hash == hash)
        {
            self.git_commit_view.open(scope.clone(), commit.clone());
            self.right_panel_width = widened_panel_width_for_review(self.right_panel_width);
        }
        cx.notify();
    }

    fn git_graph_query(&self, cx: &App) -> String {
        self.git_graph_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase()
    }

    fn ensure_git_graph_matches(
        &mut self,
        snapshot: &Arc<waku_client::git::GitGraphSnapshot>,
        cx: &App,
    ) -> Option<(Rc<Vec<bool>>, Rc<Vec<usize>>)> {
        let query = self.git_graph_query(cx);
        if query.is_empty() {
            return None;
        }
        if let Some(cache) = &self.git_graph_matches
            && Arc::ptr_eq(&cache.snapshot, snapshot)
            && cache.query == query
        {
            return Some((
                Rc::clone(&cache.matched),
                Rc::clone(&cache.match_indices),
            ));
        }
        let mut matched = Vec::with_capacity(git_graph_row_count(snapshot));
        let mut match_indices = Vec::new();
        for index in 0..git_graph_row_count(snapshot) {
            let is_match = git_graph_row_matches(snapshot, index, &query);
            if is_match {
                match_indices.push(index);
            }
            matched.push(is_match);
        }
        let matched = Rc::new(matched);
        let match_indices = Rc::new(match_indices);
        self.git_graph_matches = Some(GitGraphMatches {
            snapshot: Arc::clone(snapshot),
            query,
            matched: Rc::clone(&matched),
            match_indices: Rc::clone(&match_indices),
        });
        Some((matched, match_indices))
    }

    fn git_graph_row_matches(
        &mut self,
        snapshot: &Arc<waku_client::git::GitGraphSnapshot>,
        cx: &App,
    ) -> Option<Rc<Vec<bool>>> {
        self.ensure_git_graph_matches(snapshot, cx)
            .map(|(matched, _)| matched)
    }

    pub(super) fn on_git_graph_search_edited(&mut self, cx: &mut Context<Self>) {
        let query = self.git_graph_query(cx);
        if !query.is_empty()
            && let Some(scope) = self.selected_panel_scope()
            && let Some((visible_scope, snapshot)) = &self.visible_git_graph
            && visible_scope == &scope
        {
            let snapshot = Arc::clone(snapshot);
            if let Some((matched, match_indices)) = self.ensure_git_graph_matches(&snapshot, cx) {
                let cursor_matches = self
                    .git_graph_cursor_in(&scope)
                    .and_then(|idx| matched.get(idx).copied())
                    .unwrap_or(false);
                if !cursor_matches && let Some(&first) = match_indices.first() {
                    self.focus_git_graph_row(first, cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    fn step_git_graph_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.git_graph_query(cx).is_empty() {
            return;
        }
        let Some(scope) = self.selected_panel_scope() else {
            return;
        };
        let Some((visible_scope, snapshot)) = &self.visible_git_graph else {
            return;
        };
        if visible_scope != &scope {
            return;
        }
        let snapshot = Arc::clone(snapshot);
        let Some((_, match_indices)) = self.ensure_git_graph_matches(&snapshot, cx) else {
            return;
        };
        if match_indices.is_empty() {
            return;
        }
        let current = self.git_graph_cursor_in(&scope);
        let target = if forward {
            match current {
                Some(cursor) => match_indices
                    .iter()
                    .copied()
                    .find(|&idx| idx > cursor)
                    .unwrap_or(match_indices[0]),
                None => match_indices[0],
            }
        } else {
            match current {
                Some(cursor) => match_indices
                    .iter()
                    .copied()
                    .rev()
                    .find(|&idx| idx < cursor)
                    .unwrap_or(match_indices[match_indices.len() - 1]),
                None => match_indices[match_indices.len() - 1],
            }
        };
        self.focus_git_graph_row(target, cx);
    }

    fn clear_git_graph_search(&mut self, cx: &mut Context<Self>) {
        self.git_graph_search.update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    fn git_graph_refs_groups(
        &mut self,
        snapshot: &Arc<waku_client::git::GitGraphSnapshot>,
    ) -> (
        Rc<Vec<RefJumpItem>>,
        Rc<Vec<RefJumpItem>>,
        Rc<Vec<RefJumpItem>>,
    ) {
        if let Some(cache) = &self.git_graph_refs_cache
            && Arc::ptr_eq(&cache.snapshot, snapshot)
        {
            return (
                Rc::clone(&cache.branches),
                Rc::clone(&cache.tags),
                Rc::clone(&cache.remotes),
            );
        }
        let offset = usize::from(!snapshot.uncommitted.is_empty());
        let mut branches = Vec::new();
        let mut tags = Vec::new();
        let mut remotes = Vec::new();
        for (commit_index, commit) in snapshot.commits.iter().enumerate() {
            if commit.refs.is_empty() {
                continue;
            }
            let row_index = commit_index + offset;
            let lane_color = snapshot
                .rows
                .get(commit_index)
                .and_then(|row| row.lanes.get(row.node_lane as usize))
                .map(|lane| lane.color);
            let hash = short_hash(&commit.hash).to_owned();
            for display_ref in group_commit_refs(&commit.refs) {
                let item = RefJumpItem {
                    display_ref,
                    row_index,
                    lane_color,
                    short_hash: hash.clone(),
                };
                match item.display_ref.primary.kind {
                    waku_client::git::GitGraphRefKind::Branch
                    | waku_client::git::GitGraphRefKind::Head => branches.push(item),
                    waku_client::git::GitGraphRefKind::Tag => tags.push(item),
                    waku_client::git::GitGraphRefKind::Remote => remotes.push(item),
                }
            }
        }
        let branches = Rc::new(branches);
        let tags = Rc::new(tags);
        let remotes = Rc::new(remotes);
        self.git_graph_refs_cache = Some(GitGraphRefsCache {
            snapshot: Arc::clone(snapshot),
            branches: Rc::clone(&branches),
            tags: Rc::clone(&tags),
            remotes: Rc::clone(&remotes),
        });
        (branches, tags, remotes)
    }

    pub(super) fn render_git_graph(
        &mut self,
        panel_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
                if this.git_commit_page_open() {
                    this.step_git_commit_file(1, cx);
                } else {
                    this.step_git_graph(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousEntry, _, cx| {
                if this.git_commit_page_open() {
                    this.step_git_commit_file(-1, cx);
                } else {
                    this.step_git_graph(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &GitGraphNextMatch, _, cx| {
                if !this.git_commit_page_open() {
                    this.step_git_graph_match(true, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &GitGraphPrevMatch, _, cx| {
                if !this.git_commit_page_open() {
                    this.step_git_graph_match(false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &GitGraphTop, _, cx| {
                if this.git_commit_page_open() {
                    this.step_git_commit_file(isize::MIN, cx);
                    return;
                }
                if this
                    .selected_git_graph_row_count()
                    .is_some_and(|count| count > 0)
                {
                    this.focus_git_graph_row(0, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &GitGraphBottom, _, cx| {
                if this.git_commit_page_open() {
                    this.step_git_commit_file(isize::MAX, cx);
                    return;
                }
                if let Some(count) = this.selected_git_graph_row_count()
                    && count > 0
                {
                    this.focus_git_graph_row(count - 1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ConfirmEntry, _, cx| {
                // The commit page owns the keyboard while it is open: Enter
                // must not reopen the first row underneath it.
                if this.git_commit_page_open() {
                    return;
                }
                if let Some(cursor) = this.selected_git_graph_cursor() {
                    this.open_git_graph_row(cursor, cx);
                    return;
                }
                if this
                    .selected_git_graph_row_count()
                    .is_some_and(|count| count > 0)
                {
                    this.open_git_graph_row(0, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DismissMenu, window, cx| {
                if this.git_commit_page_open() {
                    this.close_git_commit_page(window, cx);
                    return;
                }
                // Escape steps back: first out of a comparison, then out of
                // the selection.
                if !this.clear_git_graph_compare() {
                    this.git_graph_cursor = None;
                }
                cx.notify();
            }));

        if self.git_commit_page_open() {
            return column
                .child(self.render_git_commit_page(cx))
                .into_any_element();
        }
        let ready_snapshot = match &view {
            GitGraphView::Ready(snapshot)
                if !(snapshot.commits.is_empty() && snapshot.uncommitted.is_empty()) =>
            {
                Some(Arc::clone(snapshot))
            }
            _ => None,
        };
        column = column.child(self.render_git_graph_toolbar(
            ready_snapshot.as_ref(),
            &theme,
            cx,
        ));

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
                    column.child(self.render_git_graph_ready(snapshot, panel_width, &theme, cx))
                }
            }
        };
        column.into_any_element()
    }

    fn render_git_graph_toolbar(
        &mut self,
        snapshot: Option<&Arc<waku_client::git::GitGraphSnapshot>>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let refresh_focus = self.git_graph_refresh_focus.clone();
        let query_active = !self.git_graph_query(cx).is_empty();
        let match_info = if query_active && let Some(snapshot) = snapshot {
            let cursor = self.selected_git_graph_cursor();
            let (_, indices) = self
                .ensure_git_graph_matches(snapshot, cx)
                .unwrap_or_else(|| (Rc::new(Vec::new()), Rc::new(Vec::new())));
            let total = indices.len();
            let current = cursor
                .and_then(|c| indices.iter().position(|&idx| idx == c))
                .map(|pos| pos + 1)
                .unwrap_or(0);
            Some((current, total))
        } else {
            None
        };
        let refs_jump = snapshot.map(|snapshot| self.render_git_graph_refs_jump(snapshot, theme, cx));

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
                    .flex_1()
                    .min_w(px(100.0)),
            )
            .when(query_active, |toolbar| {
                let (count_label, has_matches) = match match_info {
                    Some((current, total)) if total > 0 => (format!("{current}/{total}"), true),
                    _ => ("0".to_owned(), false),
                };
                toolbar
                    .when(match_info.is_some(), |toolbar| {
                        toolbar
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(2.0))
                                    .text_size(sp(11.0))
                                    .text_color(if has_matches {
                                        theme.text_tertiary
                                    } else {
                                        theme.danger
                                    })
                                    .child(count_label),
                            )
                            .child(toolbar_mini_icon_button(
                                "git-graph-prev-match",
                                "icons/chevron-up.svg",
                                &self.git_graph_prev_match_focus,
                                tr!("git_graph.prev_match"),
                                theme,
                                cx.listener(|this, _, _, cx| this.step_git_graph_match(false, cx)),
                                cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if !event.keystroke.modifiers.modified()
                                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                    {
                                        this.step_git_graph_match(false, cx);
                                        cx.stop_propagation();
                                    }
                                }),
                            ))
                            .child(toolbar_mini_icon_button(
                                "git-graph-next-match",
                                "icons/chevron-down.svg",
                                &self.git_graph_next_match_focus,
                                tr!("git_graph.next_match"),
                                theme,
                                cx.listener(|this, _, _, cx| this.step_git_graph_match(true, cx)),
                                cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                    if !event.keystroke.modifiers.modified()
                                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                    {
                                        this.step_git_graph_match(true, cx);
                                        cx.stop_propagation();
                                    }
                                }),
                            ))
                    })
                    .child(toolbar_mini_icon_button(
                        "git-graph-clear-search",
                        "icons/x.svg",
                        &self.git_graph_clear_search_focus,
                        tr!("git_graph.clear_filter"),
                        theme,
                        cx.listener(|this, _, _, cx| this.clear_git_graph_search(cx)),
                        cx.listener(|this, event: &KeyDownEvent, _, cx| {
                            if !event.keystroke.modifiers.modified()
                                && matches!(event.keystroke.key.as_str(), "enter" | "space")
                            {
                                this.clear_git_graph_search(cx);
                                cx.stop_propagation();
                            }
                        }),
                    ))
            })
            .when(self.git_graph_ui.pending(), |element| {
                element.child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(motion::spin(icon(
                            "icons/loader-circle.svg",
                            13.0,
                            theme.text_tertiary,
                        ))),
                )
            })
            .children(refs_jump)
            .child(
                icon_button("git-graph-fetch", "icons/download.svg", *theme)
                    .tab_index(0)
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .tooltip(Tooltip::text(tr!("git_graph.fetch")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_git_graph_action(waku_client::git::GitGraphAction::Fetch, cx);
                    })),
            )
            .child(
                icon_button("git-graph-pull", "icons/arrow-down.svg", *theme)
                    .tab_index(0)
                    .tab_stop(true)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .tooltip(Tooltip::text(tr!("git_graph.pull")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_git_graph_action(waku_client::git::GitGraphAction::Pull, cx);
                    })),
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

    fn render_git_graph_refs_jump(
        &mut self,
        snapshot: &Arc<waku_client::git::GitGraphSnapshot>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (branches, tags, remotes) = self.git_graph_refs_groups(snapshot);
        let handle = self.menu_handle("git-graph-refs-jump", cx);
        let popover_handle = handle.clone();
        let trigger = icon_button("git-graph-refs", "icons/git-branch.svg", *theme)
            .track_focus(&self.git_graph_refs_focus)
            .tab_index(0)
            .tab_stop(true)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .tooltip(Tooltip::text(tr!("git_graph.jump_to_ref")))
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    crate::ui::menu::toggle_popover(
                        &popover_handle,
                        MenuAlign::BelowRight,
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }
            });
        let weak = cx.entity().downgrade();
        let theme_copy = *theme;
        popover(trigger, &handle, MenuAlign::BelowRight, move |handle, _, cx| {
            render_refs_jump_popover(
                handle,
                &branches,
                &tags,
                &remotes,
                &theme_copy,
                weak.clone(),
                cx,
            )
        })
    }

    fn render_git_graph_ready(
        &mut self,
        snapshot: Arc<waku_client::git::GitGraphSnapshot>,
        panel_width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let scope = self.selected_panel_scope().unwrap_or_else(|| PanelScope {
            host: waku_client::hosts::HostKey::Local,
            workspace: PathBuf::new(),
        });
        let rows = git_graph_row_count(&snapshot);
        if self
            .git_graph_cursor_in(&scope)
            .is_some_and(|cursor| cursor >= rows)
        {
            self.git_graph_cursor = None;
        }
        let same_path = self.git_graph_list_path.as_ref() == Some(&scope);
        if self.git_graph_list_rows != rows || !same_path {
            // A reset scrolls to the top; keep the reader's place when the
            // same history only grew or shrank (load more, a new commit).
            let keep = same_path.then(|| self.git_graph_list_state.logical_scroll_top());
            self.git_graph_list_rows = rows;
            self.git_graph_list_path = Some(scope.clone());
            self.git_graph_list_state
                .reset_with_uniform_height(rows, px(ROW_HEIGHT));
            if let Some(top) = keep {
                self.git_graph_list_state.scroll_to(top);
            }
        }
        let cursor = self.git_graph_cursor_in(&scope);
        let head = snapshot
            .head_row
            .and_then(|row| snapshot.rows.get(row as usize));
        let cursor_hash = cursor
            .and_then(|cursor| git_graph_row_key(&snapshot, cursor))
            .and_then(|key| match key {
                GitGraphRowKey::Commit(hash) => Some(hash),
                GitGraphRowKey::Uncommitted => None,
            });
        let layout =
            compute_graph_layout(panel_width, snapshot.lane_count, self.git_graph_column_ratio);
        let context = GitGraphRowContext {
            matches: self.git_graph_row_matches(&snapshot, cx),
            cursor,
            cursor_hash,
            compare: self.git_graph_compare_hash().map(str::to_owned),
            head_lane: head.map_or(0, |row| row.node_lane),
            head_on_top: snapshot.head_row == Some(0),
            panel_width,
            layout,
            now: unix_time() as i64,
            snapshot: Arc::clone(&snapshot),
        };
        let entity = cx.entity().downgrade();
        let divider_x = ROW_LEFT_PAD + layout.column_width;
        let dragging_col = self
            .panel_resize_drag
            .is_some_and(|drag| drag.target == PanelResizeTarget::GitGraphColumn);

        let column_header = div()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .text_size(sp(11.0))
            .text_color(theme.text_tertiary)
            .child(
                div()
                    .w(px(divider_x))
                    .h_full()
                    .pl(px(10.0))
                    .pr(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(theme.border)
                    .truncate()
                    .child(tr_cow!("git_graph.column_graph")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().truncate().child(tr_cow!("git_graph.column_commit")))
                    .when(panel_width >= 520.0, |header| {
                        header.child(div().flex_none().child(tr_cow!("git_graph.column_meta")))
                    }),
            );

        let resize_handle = div()
            .id("git-graph-col-resize-handle")
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(divider_x - 5.0))
            .w(px(10.0))
            .group("git-graph-col-resize")
            .cursor_col_resize()
            .tooltip(Tooltip::text(tr!("git_graph.resize_graph_hint")))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(4.0))
                    .w(px(2.0))
                    .h_full()
                    .bg(if dragging_col {
                        theme.resize_handle
                    } else {
                        gpui::transparent_black()
                    })
                    .group_hover("git-graph-col-resize", |element| {
                        element.bg(theme.resize_handle)
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if event.click_count >= 2 {
                        this.git_graph_column_ratio = None;
                        cx.notify();
                        cx.stop_propagation();
                        return;
                    }
                    this.begin_panel_resize(
                        PanelResizeTarget::GitGraphColumn,
                        event,
                        window,
                        cx,
                    );
                }),
            );

        let mut body = div().flex_1().min_h_0().flex().flex_col().child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .flex()
                .flex_col()
                .child(column_header)
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .track_focus(&self.git_graph_focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        // Shift+F10 / the Menu key open the selected row's context
                        // menu, so every action stays reachable without a mouse.
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            let key = event.keystroke.key.as_str();
                            let menu_key = (key == "f10" && event.keystroke.modifiers.shift)
                                || (key == "menu" && !event.keystroke.modifiers.modified());
                            if !menu_key {
                                return;
                            }
                            if let Some(cursor) = this.selected_git_graph_cursor() {
                                this.menu_handle(format!("git-graph-row-{cursor}"), cx)
                                    .open_context_menu(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            list(
                                self.git_graph_list_state.clone(),
                                move |index, _window, cx| {
                                    entity
                                        .upgrade()
                                        .map(|entity| {
                                            entity.update(cx, |this, cx| {
                                                this.git_graph_row(index, &context, cx)
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
                )
                .child(resize_handle),
        );
        if let Some(cursor) = cursor
            && (self.git_graph_compare_hash().is_some()
                || matches!(
                    git_graph_row_key(&snapshot, cursor),
                    Some(GitGraphRowKey::Uncommitted)
                ))
        {
            body = body.child(self.render_git_graph_detail(cursor, &snapshot, theme, cx));
        }
        if snapshot.truncated && self.git_graph_page_limit(&scope) < MAX_LIMIT {
            body = body.child(self.render_git_graph_load_more(theme, cx));
        }
        body
    }

    fn git_graph_row(
        &self,
        index: usize,
        context: &GitGraphRowContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let snapshot = &context.snapshot;
        let selected = context.cursor == Some(index);
        let matched = context
            .matches
            .as_ref()
            .is_none_or(|matches| matches.get(index).copied().unwrap_or(false));
        let has_uncommitted = !snapshot.uncommitted.is_empty();
        let is_uncommitted = has_uncommitted && index == 0;
        let commit_index = index.checked_sub(usize::from(has_uncommitted));
        let commit = if is_uncommitted {
            None
        } else {
            commit_index.and_then(|pos| snapshot.commits.get(pos))
        };
        let row = if is_uncommitted {
            None
        } else {
            commit_index.and_then(|pos| snapshot.rows.get(pos))
        };
        let is_head_commit = !is_uncommitted
            && snapshot
                .head_row
                .is_some_and(|head_row| Some(head_row as usize) == commit_index);
        let is_merge = commit.is_some_and(|commit| commit.parents.len() > 1);
        let node_color = row.and_then(|row| {
            row.lanes
                .get(row.node_lane as usize)
                .map(|lane| lane.color)
        });

        let label = match commit {
            Some(commit) => commit.subject.clone(),
            None => tr!("git_graph.uncommitted"),
        };
        let time_ago = commit.map(|commit| {
            let seconds = context.now.saturating_sub(commit.authored_at).max(0) as u64;
            super::sidebar::format_time_ago(seconds)
        });
        let short_id = commit.map(|commit| short_hash(&commit.hash).to_owned());

        let (max_chips, max_chip_label_width) = responsive_ref_budget(context.panel_width);
        let grouped_refs = commit
            .map(|commit| group_commit_refs(&commit.refs))
            .unwrap_or_default();
        let hidden_refs = grouped_refs.len().saturating_sub(max_chips);
        // The dashed link from the uncommitted row can only be drawn when
        // HEAD is the row right below it.
        let connect_above = has_uncommitted && index == 1 && context.head_on_top;
        let compared =
            commit.is_some_and(|commit| context.compare.as_deref() == Some(commit.hash.as_str()));
        let row_bg = if selected {
            blend_over(theme.overlay_strong, theme.surface)
        } else if compared {
            blend_over(theme.overlay, theme.surface)
        } else {
            theme.surface
        };
        let weak = cx.entity().downgrade();
        let hash = commit.map(|commit| commit.hash.clone());
        let chips = grouped_refs
            .iter()
            .take(max_chips)
            .enumerate()
            .map(|(position, display_ref)| {
                let handle = self.menu_handle(format!("git-graph-ref-{index}-{position}"), cx);
                let weak = weak.clone();
                let menu_ref = display_ref.clone();
                let menu_hash = hash.clone().unwrap_or_default();
                context_menu(
                    git_ref_chip(
                        display_ref,
                        node_color,
                        Some(max_chip_label_width),
                        &theme,
                    ),
                    SharedString::from(format!("git-graph-ref-menu-{index}-{position}")),
                    &handle,
                    move |_| Waku::git_graph_display_ref_menu(weak.clone(), &menu_ref, &menu_hash),
                )
            })
            .collect::<Vec<_>>();
        let overflow = (hidden_refs > 0).then(|| {
            let handle = self.menu_handle(format!("git-graph-ref-{index}-more"), cx);
            let weak = weak.clone();
            let hidden = grouped_refs[max_chips..].to_vec();
            let menu_hash = hash.clone().unwrap_or_default();
            context_menu(
                overflow_chip(hidden_refs, &theme),
                SharedString::from(format!("git-graph-ref-menu-{index}-more")),
                &handle,
                move |_| {
                    hidden
                        .iter()
                        .map(|display_ref| {
                            let weak = weak.clone();
                            let display_ref = display_ref.clone();
                            let menu_hash = menu_hash.clone();
                            MenuItem::Submenu {
                                label: display_ref.primary.name.clone().into(),
                                value: None,
                                items: Rc::new(move |_| {
                                    Waku::git_graph_display_ref_menu(
                                        weak.clone(),
                                        &display_ref,
                                        &menu_hash,
                                    )
                                }),
                            }
                        })
                        .collect()
                },
            )
        });
        let click_hash = hash.clone();

        let element = div()
            .id(SharedString::from(format!("git-graph-row-{index}")))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .pl(px(ROW_LEFT_PAD))
            .pr(px(8.0))
            .cursor_default()
            .when(!matched, |element| element.opacity(0.38))
            .when(selected, |element| element.bg(theme.overlay_strong))
            .when(compared && !selected, |element| element.bg(theme.overlay))
            .when(!selected && !compared, |element| {
                element.hover(|element| element.bg(theme.overlay))
            })
            .child(git_graph_lanes(
                row.cloned(),
                is_uncommitted,
                connect_above,
                context.head_lane,
                context.head_on_top,
                is_head_commit,
                is_merge,
                row_bg,
                context.layout,
                &theme,
            ))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .pl(px(10.0))
                    .pr(px(4.0))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .min_w_0()
                            .overflow_hidden()
                            .children(chips)
                            .children(overflow)
                            .child(
                                div()
                                    .min_w(px(64.0))
                                    .flex_1()
                                    .text_size(sp(12.5))
                                    .line_height(sp(16.0))
                                    .font_weight(if selected || is_head_commit {
                                        FontWeight::MEDIUM
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(theme.text)
                                    .truncate()
                                    .child(label),
                            )
                            // Compared rows say so in text too, not only with a tint.
                            .when(compared, |element| {
                                element.child(
                                    div()
                                        .flex_none()
                                        .h(px(16.0))
                                        .px(px(5.0))
                                        .rounded(px(4.0))
                                        .bg(theme.accent.opacity(0.14))
                                        .flex()
                                        .items_center()
                                        .text_size(sp(10.0))
                                        .text_color(theme.accent)
                                        .child(tr!("git_graph.compare_badge")),
                                )
                            }),
                    )
                    .when_some(commit, |element, commit| {
                        let time_ago = time_ago.unwrap_or_default();
                        let short_id = short_id.unwrap_or_default();
                        element.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(8.0))
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .min_w_0()
                                        .flex_1()
                                        .child(
                                            div()
                                                .min_w_0()
                                                .text_size(sp(11.5))
                                                .line_height(sp(14.0))
                                                .text_color(theme.text_tertiary)
                                                .truncate()
                                                .child(commit.author.clone()),
                                        )
                                        .when(is_merge, |meta| {
                                            meta.child(
                                                div()
                                                    .flex_none()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(3.0))
                                                    .text_size(sp(10.5))
                                                    .line_height(sp(14.0))
                                                    .text_color(theme.text_tertiary)
                                                    .child(icon(
                                                        "icons/fork.svg",
                                                        10.5,
                                                        theme.text_tertiary,
                                                    ))
                                                    .child(tr_cow!("git_graph.merge_badge")),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap(px(5.0))
                                        .text_size(sp(11.0))
                                        .line_height(sp(14.0))
                                        .text_color(theme.text_tertiary)
                                        .child(time_ago)
                                        .child("·")
                                        .child(
                                            div()
                                                .font_family(md::render::MONO_FAMILY)
                                                .child(short_id),
                                        ),
                                ),
                        )
                    }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                // The primary modifier picks a second commit to compare with
                // the selected one, as in the Git Graph extension.
                if event.modifiers().secondary()
                    && let Some(hash) = &click_hash
                    && this
                        .selected_git_graph_cursor()
                        .is_some_and(|cursor| cursor != index)
                {
                    this.toggle_git_graph_compare(hash.clone(), cx);
                } else {
                    this.clear_git_graph_compare();
                    this.open_git_graph_row(index, cx);
                }
                window.focus(&this.git_graph_focus, cx);
            }));

        let handle = self.menu_handle(format!("git-graph-row-{index}"), cx);
        let target = commit.map(|commit| super::git_graph_actions::CommitMenuTarget {
            index,
            hash: commit.hash.clone(),
            subject: commit.subject.clone(),
            compare_with: context
                .cursor_hash
                .clone()
                .filter(|cursor| *cursor != commit.hash),
        });
        context_menu(
            element,
            SharedString::from(format!("git-graph-row-menu-{index}")),
            &handle,
            move |_| match &target {
                Some(target) => Waku::git_graph_commit_menu(weak.clone(), target.clone()),
                None => Waku::git_graph_uncommitted_menu(weak.clone()),
            },
        )
    }

    pub(super) fn render_git_graph_hash(
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

fn toolbar_mini_icon_button(
    id: &'static str,
    icon_path: &'static str,
    focus: &FocusHandle,
    tooltip: String,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_key_down: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .track_focus(focus)
        .tab_index(0)
        .tab_stop(true)
        .flex_none()
        .size(px(20.0))
        .rounded(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_default()
        .hover(|element| element.bg(theme.overlay))
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .tooltip(Tooltip::text(tooltip))
        .child(icon(icon_path, 12.0, theme.text_secondary))
        .on_click(on_click)
        .on_key_down(on_key_down)
}

fn render_refs_jump_popover(
    handle: &ContextMenuHandle,
    branches: &[RefJumpItem],
    tags: &[RefJumpItem],
    remotes: &[RefJumpItem],
    theme: &Theme,
    weak: WeakEntity<Waku>,
    cx: &mut App,
) -> AnyElement {
    let mut card = div()
        .id("git-graph-refs-popover")
        .w(px(280.0))
        .max_h(px(340.0))
        .overflow_y_scroll()
        .py(px(6.0))
        .rounded(px(9.0))
        .border_1()
        .border_color(theme.border_strong)
        .bg(theme.raised)
        .shadow_lg()
        .flex()
        .flex_col();

    if branches.is_empty() && tags.is_empty() && remotes.is_empty() {
        return card
            .child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("git_graph.no_refs")),
            )
            .into_any_element();
    }

    let sections = [
        ("branches", tr!("git_graph.refs_branches"), branches),
        ("tags", tr!("git_graph.refs_tags"), tags),
        ("remotes", tr!("git_graph.refs_remotes"), remotes),
    ];

    for (section_id, title, items) in sections {
        if items.is_empty() {
            continue;
        }
        card = card.child(
            div()
                .px(px(10.0))
                .pt(px(6.0))
                .pb(px(3.0))
                .text_size(sp(10.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_tertiary)
                .child(title),
        );
        for (pos, item) in items.iter().enumerate() {
            let row_index = item.row_index;
            let focus = cx.focus_handle();
            let weak_click = weak.clone();
            let handle_click = handle.clone();
            let weak_key = weak.clone();
            let handle_key = handle.clone();
            card = card.child(
                div()
                    .id(SharedString::from(format!(
                        "git-graph-jump-{section_id}-{pos}"
                    )))
                    .track_focus(&focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .h(px(28.0))
                    .mx(px(4.0))
                    .px(px(6.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .cursor_default()
                    .hover(|element| element.bg(theme.overlay))
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .child(git_ref_chip(
                                &item.display_ref,
                                item.lane_color,
                                Some(180.0),
                                theme,
                            )),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(md::render::MONO_FAMILY)
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .child(item.short_hash.clone()),
                    )
                    .on_click(move |_, window, cx| {
                        handle_click.close(window, cx);
                        let _ = weak_click.update(cx, |this, cx| {
                            this.focus_git_graph_row(row_index, cx);
                            window.focus(&this.git_graph_focus, cx);
                        });
                    })
                    .on_key_down(move |event: &KeyDownEvent, window, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            handle_key.close(window, cx);
                            let _ = weak_key.update(cx, |this, cx| {
                                this.focus_git_graph_row(row_index, cx);
                                window.focus(&this.git_graph_focus, cx);
                            });
                            cx.stop_propagation();
                        }
                    }),
            );
        }
    }
    card.into_any_element()
}

pub(super) fn compute_graph_layout(
    panel_width: f32,
    lane_count: u16,
    manual_ratio: Option<f32>,
) -> GraphLayout {
    const MIN_LANE_PITCH: f32 = 14.0;
    let panel_width = panel_width.max(320.0);
    let lanes = lane_count.max(1) as f32;
    let width_t = ((panel_width - 360.0) / (1120.0 - 360.0)).clamp(0.0, 1.0);
    let pad_x = (10.0 + width_t * 6.0).round();

    let (column_width, lane_pitch, canvas_width) = match manual_ratio {
        None => {
            let ideal_pitch = 15.5 + width_t * 10.5;
            let min_commit_width = (210.0 + width_t * 110.0).min(panel_width * 0.55);
            let max_budget = (panel_width * (0.36 + width_t * 0.08))
                .min(panel_width - min_commit_width)
                .max(GIT_GRAPH_MIN_COLUMN_WIDTH);
            let available = (max_budget - pad_x * 2.0).max(MIN_LANE_PITCH);
            let lane_pitch = (available / lanes).clamp(MIN_LANE_PITCH, ideal_pitch);
            let canvas_width = pad_x * 2.0 + lanes * lane_pitch;
            let column_width = canvas_width
                .clamp(GIT_GRAPH_MIN_COLUMN_WIDTH, max_budget)
                .round();
            (column_width, lane_pitch, canvas_width.max(column_width))
        }
        Some(ratio) => {
            let max_col = (panel_width - GIT_GRAPH_MIN_COMMIT_WIDTH)
                .max(GIT_GRAPH_MIN_COLUMN_WIDTH)
                .min(panel_width * 0.62);
            let column_width = (panel_width * ratio)
                .clamp(GIT_GRAPH_MIN_COLUMN_WIDTH, max_col)
                .round();
            let available = (column_width - pad_x * 2.0).max(MIN_LANE_PITCH);
            let lane_pitch = (available / lanes).clamp(MIN_LANE_PITCH, 32.0);
            let canvas_width = (pad_x * 2.0 + lanes * lane_pitch).max(column_width);
            (column_width, lane_pitch, canvas_width)
        }
    };

    let pitch_t = ((lane_pitch - MIN_LANE_PITCH) / (26.0 - MIN_LANE_PITCH)).clamp(0.0, 1.0);
    let node_radius = 4.25 + pitch_t * 1.0;
    let stroke_width = 1.6 + pitch_t * 0.45;
    GraphLayout {
        column_width,
        canvas_width,
        pad_x,
        lane_pitch,
        node_radius,
        stroke_width,
    }
}

fn responsive_ref_budget(panel_width: f32) -> (usize, f32) {
    if panel_width < 460.0 {
        (2, 110.0)
    } else if panel_width < 720.0 {
        (3, 165.0)
    } else if panel_width < 980.0 {
        (4, 220.0)
    } else {
        (5, 280.0)
    }
}

pub(super) fn group_commit_refs(
    refs: &[waku_client::git::GitGraphRef],
) -> Vec<DisplayGitRef> {
    if refs.is_empty() {
        return Vec::new();
    }
    let mut consumed = vec![false; refs.len()];
    let mut grouped = Vec::with_capacity(refs.len());

    for (index, git_ref) in refs.iter().enumerate() {
        if consumed[index] {
            continue;
        }
        if git_ref.kind == waku_client::git::GitGraphRefKind::Branch {
            let suffix = format!("/{}", git_ref.name);
            let mut synced_remotes = Vec::new();
            for (remote_idx, candidate) in refs.iter().enumerate() {
                if !consumed[remote_idx]
                    && candidate.kind == waku_client::git::GitGraphRefKind::Remote
                    && let Some(prefix) = candidate.name.strip_suffix(&suffix)
                    && !prefix.is_empty()
                    && !prefix.contains('/')
                {
                    consumed[remote_idx] = true;
                    synced_remotes.push(candidate.clone());
                }
            }
            grouped.push(DisplayGitRef {
                primary: git_ref.clone(),
                synced_remotes,
            });
        } else {
            grouped.push(DisplayGitRef {
                primary: git_ref.clone(),
                synced_remotes: Vec::new(),
            });
        }
    }
    grouped
}

fn git_graph_row_count(snapshot: &waku_client::git::GitGraphSnapshot) -> usize {
    snapshot.commits.len() + usize::from(!snapshot.uncommitted.is_empty())
}

fn git_graph_row_key(
    snapshot: &waku_client::git::GitGraphSnapshot,
    index: usize,
) -> Option<GitGraphRowKey> {
    let uncommitted = !snapshot.uncommitted.is_empty();
    if uncommitted && index == 0 {
        return Some(GitGraphRowKey::Uncommitted);
    }
    snapshot
        .commits
        .get(index - usize::from(uncommitted))
        .map(|commit| GitGraphRowKey::Commit(commit.hash.clone()))
}

fn git_graph_row_index(
    snapshot: &waku_client::git::GitGraphSnapshot,
    key: &GitGraphRowKey,
) -> Option<usize> {
    let uncommitted = !snapshot.uncommitted.is_empty();
    match key {
        GitGraphRowKey::Uncommitted => uncommitted.then_some(0),
        GitGraphRowKey::Commit(hash) => snapshot
            .commits
            .iter()
            .position(|commit| &commit.hash == hash)
            .map(|index| index + usize::from(uncommitted)),
    }
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

pub(super) fn ref_chip_style(
    theme: &Theme,
    kind: waku_client::git::GitGraphRefKind,
    current: bool,
    lane_color: Option<u8>,
) -> RefChipStyle {
    let color_index = lane_color.unwrap_or(0);
    let hue = LANE_HUES[(color_index as usize) % LANE_HUES.len()] / 360.0;
    let lane = lane_hsla(theme, color_index);
    match kind {
        waku_client::git::GitGraphRefKind::Tag => RefChipStyle {
            bg: theme.warning.opacity(0.14),
            border: theme.warning.opacity(0.52),
            fg: hsla(
                38.0 / 360.0,
                0.82,
                if theme.is_dark { 0.78 } else { 0.30 },
                1.0,
            ),
            icon: theme.warning,
            badge_bg: theme.warning.opacity(0.24),
        },
        waku_client::git::GitGraphRefKind::Head => RefChipStyle {
            bg: theme.accent.opacity(0.18),
            border: theme.accent.opacity(0.68),
            fg: if theme.is_dark {
                hsla(14.0 / 360.0, 0.80, 0.82, 1.0)
            } else {
                theme.accent
            },
            icon: theme.accent,
            badge_bg: theme.accent.opacity(0.26),
        },
        waku_client::git::GitGraphRefKind::Branch if current => RefChipStyle {
            bg: lane.opacity(if theme.is_dark { 0.24 } else { 0.16 }),
            border: lane.opacity(0.80),
            fg: hsla(hue, 0.78, if theme.is_dark { 0.86 } else { 0.26 }, 1.0),
            icon: lane,
            badge_bg: lane.opacity(0.28),
        },
        waku_client::git::GitGraphRefKind::Branch => RefChipStyle {
            bg: lane.opacity(if theme.is_dark { 0.14 } else { 0.10 }),
            border: lane.opacity(0.48),
            fg: hsla(hue, 0.70, if theme.is_dark { 0.82 } else { 0.28 }, 1.0),
            icon: lane,
            badge_bg: lane.opacity(0.20),
        },
        waku_client::git::GitGraphRefKind::Remote => RefChipStyle {
            bg: lane.opacity(if theme.is_dark { 0.07 } else { 0.06 }),
            border: lane.opacity(0.30),
            fg: hsla(hue, 0.38, if theme.is_dark { 0.72 } else { 0.36 }, 1.0),
            icon: lane.opacity(0.80),
            badge_bg: lane.opacity(0.14),
        },
    }
}

pub(super) fn git_ref_chip(
    display_ref: &DisplayGitRef,
    lane_color: Option<u8>,
    max_label_width: Option<f32>,
    theme: &Theme,
) -> Stateful<Div> {
    let primary = &display_ref.primary;
    let style = ref_chip_style(theme, primary.kind, primary.current, lane_color);
    let icon_path = match primary.kind {
        waku_client::git::GitGraphRefKind::Branch => "icons/git-branch.svg",
        waku_client::git::GitGraphRefKind::Remote => "icons/globe.svg",
        waku_client::git::GitGraphRefKind::Tag => "icons/tag.svg",
        waku_client::git::GitGraphRefKind::Head => "icons/target.svg",
    };
    let tooltip_text = if display_ref.synced_remotes.is_empty() {
        primary.name.clone()
    } else {
        let remotes = display_ref
            .synced_remotes
            .iter()
            .map(|remote| remote.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} · {remotes}", primary.name)
    };
    let show_head_badge =
        primary.current && primary.kind != waku_client::git::GitGraphRefKind::Head;

    div()
        .id(SharedString::from(format!(
            "git-ref-chip-{}-{:?}",
            primary.name, primary.kind
        )))
        .flex_none()
        .h(px(20.0))
        .px(px(6.0))
        .rounded(px(5.0))
        .border_1()
        .border_color(style.border)
        .bg(style.bg)
        .flex()
        .items_center()
        .gap(px(4.0))
        .text_size(sp(11.0))
        .line_height(sp(14.0))
        .font_weight(if primary.current {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::MEDIUM
        })
        .text_color(style.fg)
        .tooltip(Tooltip::text(tooltip_text))
        .child(icon(icon_path, 11.0, style.icon))
        .child(
            div()
                .when_some(max_label_width, |el, width| el.max_w(px(width)).truncate())
                .child(primary.name.clone()),
        )
        .when(!display_ref.synced_remotes.is_empty(), |element| {
            element.child(icon("icons/globe.svg", 10.0, style.icon.opacity(0.85)))
        })
        .when(show_head_badge, |element| {
            element.child(
                div()
                    .px(px(4.0))
                    .h(px(14.0))
                    .rounded(px(3.0))
                    .bg(style.badge_bg)
                    .flex()
                    .items_center()
                    .text_size(sp(9.5))
                    .font_weight(FontWeight::BOLD)
                    .child(tr_cow!("git_graph.head")),
            )
        })
}

fn overflow_chip(hidden: usize, theme: &Theme) -> Div {
    div()
        .flex_none()
        .h(px(20.0))
        .px(px(6.0))
        .rounded(px(5.0))
        .border_1()
        .border_color(theme.border_strong)
        .bg(theme.overlay)
        .flex()
        .items_center()
        .text_size(sp(11.0))
        .line_height(sp(14.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_secondary)
        .child(format!("+{hidden}"))
}

pub(super) fn status_letter(status: waku_client::git::GitGraphFileStatus) -> &'static str {
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

pub(super) fn message_body<'a>(subject: &str, body: &'a str) -> Option<&'a str> {
    let body = body.trim();
    let rest = body.strip_prefix(subject).unwrap_or(body).trim();
    if rest.is_empty() { None } else { Some(rest) }
}

pub(super) fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(7)]
}

pub(super) fn short_hashes(parents: &[String]) -> String {
    parents
        .iter()
        .map(|hash| short_hash(hash))
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(clippy::too_many_arguments)]
fn git_graph_lanes(
    row: Option<waku_client::git::GitGraphRow>,
    uncommitted: bool,
    connect_above: bool,
    head_lane: u16,
    head_on_top: bool,
    is_head: bool,
    is_merge: bool,
    row_bg: Hsla,
    layout: GraphLayout,
    theme: &Theme,
) -> Div {
    let clipped = layout.canvas_width > layout.column_width + 1.0;
    let painted = canvas(
        move |_, _, _| row,
        move |bounds, row, window, cx| {
            let theme = Theme::current(cx);
            if uncommitted {
                paint_uncommitted_marker(
                    bounds,
                    head_lane,
                    head_on_top,
                    theme.text_ghost,
                    &layout,
                    window,
                );
                return;
            }
            let Some(row) = row else {
                return;
            };
            paint_commit_row(
                bounds,
                &row,
                connect_above,
                is_head,
                is_merge,
                row_bg,
                theme.text_ghost,
                &layout,
                &theme,
                window,
            );
        },
    )
    .w(px(layout.canvas_width))
    .h(px(ROW_HEIGHT))
    .flex_none();

    div()
        .flex_none()
        .relative()
        .w(px(layout.column_width))
        .h(px(ROW_HEIGHT))
        .overflow_hidden()
        .border_r_1()
        .border_color(theme.border)
        .child(painted)
        .when(clipped, |cell| {
            cell.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(14.0))
                    .bg(linear_gradient(
                        90.0,
                        linear_color_stop(row_bg.opacity(0.0), 0.0),
                        linear_color_stop(row_bg, 1.0),
                    )),
            )
        })
}

fn lane_x(origin_x: Pixels, layout: &GraphLayout, lane: u16) -> Pixels {
    origin_x + px(layout.pad_x + lane as f32 * layout.lane_pitch + layout.lane_pitch * 0.5)
}

fn lane_hsla(theme: &Theme, color: u8) -> Hsla {
    let hue = LANE_HUES[(color as usize) % LANE_HUES.len()] / 360.0;
    let (saturation, lightness) = if theme.is_dark {
        (0.72, 0.66)
    } else {
        (0.68, 0.42)
    };
    hsla(hue, saturation, lightness, 1.0)
}

fn blend_over(fg: Hsla, bg: Hsla) -> Hsla {
    let fg: Rgba = fg.into();
    let bg: Rgba = bg.into();
    let inv = 1.0 - fg.a;
    Hsla::from(Rgba {
        r: fg.r * fg.a + bg.r * inv,
        g: fg.g * fg.a + bg.g * inv,
        b: fg.b * fg.a + bg.b * inv,
        a: 1.0,
    })
}

#[allow(clippy::too_many_arguments)]
fn paint_commit_row(
    bounds: gpui::Bounds<Pixels>,
    row: &waku_client::git::GitGraphRow,
    connect_above: bool,
    is_head: bool,
    is_merge: bool,
    row_bg: Hsla,
    ghost: Hsla,
    layout: &GraphLayout,
    theme: &Theme,
    window: &mut Window,
) {
    let top = bounds.origin.y;
    let bottom = top + bounds.size.height;
    let mid = top + bounds.size.height / 2.0;
    let radius = px(layout.node_radius);
    let stroke = px(layout.stroke_width);

    for (index, lane) in row.lanes.iter().enumerate() {
        if lane.node {
            continue;
        }
        let x = lane_x(bounds.origin.x, layout, index as u16);
        let y0 = if lane.up { top } else { mid };
        let y1 = if lane.down { bottom } else { mid };
        if y1 > y0 {
            stroke_line(window, x, y0, x, y1, stroke, lane_hsla(theme, lane.color));
        }
    }

    for edge in &row.edges {
        let x0 = lane_x(bounds.origin.x, layout, edge.from);
        let x1 = lane_x(bounds.origin.x, layout, edge.to);
        let color = lane_hsla(theme, edge.color);
        let single_corner_arch = row.lanes.get(edge.from as usize).is_some_and(|l| l.node)
            && !row.lanes.get(edge.to as usize).is_some_and(|l| l.up);
        let low = edge.from.min(edge.to) as usize;
        let high = edge.from.max(edge.to) as usize;
        let crosses_active = (low + 1..high).any(|idx| {
            row.lanes
                .get(idx)
                .is_some_and(|lane| lane.up || lane.down || lane.node)
        });
        if crosses_active
            && let Some(halo) =
                build_edge_path(x0, x1, mid, bottom, single_corner_arch, stroke + px(3.0))
        {
            window.paint_path(halo, row_bg);
        }
        if let Some(path) = build_edge_path(x0, x1, mid, bottom, single_corner_arch, stroke) {
            window.paint_path(path, color);
        }
    }

    for (index, lane) in row.lanes.iter().enumerate() {
        if !lane.node {
            continue;
        }
        let x = lane_x(bounds.origin.x, layout, index as u16);
        let color = lane_hsla(theme, lane.color);
        if lane.up {
            stroke_line(window, x, top, x, mid - radius, stroke, color);
        } else if connect_above {
            // The uncommitted marker sits in the row above. Continue its
            // dashed stem down to this commit's dot.
            paint_dashed_vertical(window, x, top, mid - radius, ghost);
        }
        if lane.down {
            stroke_line(window, x, mid + radius, x, bottom, stroke, color);
        }
        fill_circle(window, point(x, mid), radius + px(1.75), row_bg);
        if is_head {
            let mut outer = PathBuilder::stroke(px(1.5));
            stroke_circle(&mut outer, point(x, mid), radius + px(2.0));
            if let Ok(path) = outer.build() {
                window.paint_path(path, color);
            }
            fill_circle(window, point(x, mid), (radius - px(0.4)).max(px(2.5)), color);
        } else if is_merge {
            fill_circle(window, point(x, mid), radius + px(0.4), color);
            let inner_radius = (radius - stroke + px(0.2)).max(px(1.8));
            fill_circle(window, point(x, mid), inner_radius, row_bg);
        } else {
            fill_circle(window, point(x, mid), radius, color);
        }
    }
}

fn build_edge_path(
    x0: Pixels,
    x1: Pixels,
    mid: Pixels,
    bottom: Pixels,
    single_corner_arch: bool,
    stroke_width: Pixels,
) -> Option<gpui::Path<Pixels>> {
    const KAPPA: f32 = 0.55228475;
    let fx0 = f32::from(x0);
    let fx1 = f32::from(x1);
    let fmid = f32::from(mid);
    let fbot = f32::from(bottom);
    let dx = fx1 - fx0;
    let mut path = PathBuilder::stroke(stroke_width);

    if dx.abs() < 0.5 {
        path.move_to(point(x0, mid));
        path.line_to(point(x0, bottom));
        return path.build().ok();
    }

    let sx = dx.signum();
    if single_corner_arch {
        let dy = (fbot - fmid).max(0.0);
        let r = (dx.abs() - 2.0).min(dy - 2.0).clamp(0.0, 10.0);
        path.move_to(point(x0, mid));
        if r < 1.0 {
            path.line_to(point(x1, mid));
            path.line_to(point(x1, bottom));
        } else {
            let corner_x = fx1 - sx * r;
            path.line_to(point(px(corner_x), mid));
            path.cubic_bezier_to(
                point(x1, px(fmid + r)),
                point(px(corner_x + sx * r * KAPPA), mid),
                point(x1, px(fmid + r * (1.0 - KAPPA))),
            );
            path.line_to(point(x1, bottom));
        }
        return path.build().ok();
    }

    let y_turn = fmid + (fbot - fmid) * 0.56;
    let r = (dx.abs() * 0.5)
        .min(y_turn - fmid - 1.0)
        .min(fbot - y_turn - 1.0)
        .clamp(0.0, 9.0);
    path.move_to(point(x0, mid));
    if r < 1.0 {
        path.line_to(point(x0, px(y_turn)));
        path.line_to(point(x1, px(y_turn)));
        path.line_to(point(x1, bottom));
    } else {
        path.line_to(point(x0, px(y_turn - r)));
        path.cubic_bezier_to(
            point(px(fx0 + sx * r), px(y_turn)),
            point(x0, px(y_turn - r * (1.0 - KAPPA))),
            point(px(fx0 + sx * r * (1.0 - KAPPA)), px(y_turn)),
        );
        path.line_to(point(px(fx1 - sx * r), px(y_turn)));
        path.cubic_bezier_to(
            point(x1, px(y_turn + r)),
            point(px(fx1 - sx * r * (1.0 - KAPPA)), px(y_turn)),
            point(x1, px(y_turn + r * (1.0 - KAPPA))),
        );
        path.line_to(point(x1, bottom));
    }
    path.build().ok()
}

fn paint_uncommitted_marker(
    bounds: gpui::Bounds<Pixels>,
    lane: u16,
    stem: bool,
    color: Hsla,
    layout: &GraphLayout,
    window: &mut Window,
) {
    let x = lane_x(bounds.origin.x, layout, lane);
    let mid = bounds.origin.y + bounds.size.height / 2.0;
    let bottom = bounds.origin.y + bounds.size.height;
    let ring_r = px((layout.node_radius - 0.5).max(3.5));
    if stem {
        paint_dashed_vertical(window, x, mid + ring_r + px(2.0), bottom, color);
    }
    let mut ring = PathBuilder::stroke(px(1.25));
    stroke_circle(&mut ring, point(x, mid), ring_r);
    if let Ok(path) = ring.build() {
        window.paint_path(path, color);
    }
}

fn paint_dashed_vertical(window: &mut Window, x: Pixels, y0: Pixels, y1: Pixels, color: Hsla) {
    let mut line = PathBuilder::stroke(px(1.25)).dash_array(&[px(2.0), px(3.0)]);
    line.move_to(point(x, y0));
    line.line_to(point(x, y1));
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
}

fn stroke_line(
    window: &mut Window,
    x0: Pixels,
    y0: Pixels,
    x1: Pixels,
    y1: Pixels,
    width: Pixels,
    color: Hsla,
) {
    let mut path = PathBuilder::stroke(width);
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

#[cfg(test)]
mod tests {
    use super::*;
    use waku_client::git::{GitGraphRef, GitGraphRefKind};

    #[test]
    fn graph_layout_scales_up_with_panel_width() {
        for lanes in [3_u16, 8_u16] {
            let narrow = compute_graph_layout(360.0, lanes, None);
            let medium = compute_graph_layout(600.0, lanes, None);
            let wide = compute_graph_layout(900.0, lanes, None);

            assert!(medium.lane_pitch > narrow.lane_pitch);
            assert!(wide.lane_pitch > medium.lane_pitch);
            assert!(medium.column_width > narrow.column_width);
            assert!(wide.column_width > medium.column_width);
            assert!(360.0 - narrow.column_width >= GIT_GRAPH_MIN_COMMIT_WIDTH);
            assert!(600.0 - medium.column_width >= GIT_GRAPH_MIN_COMMIT_WIDTH);
            assert!(900.0 - wide.column_width >= GIT_GRAPH_MIN_COMMIT_WIDTH);
        }
    }

    #[test]
    fn graph_layout_manual_ratio_scales_proportionally_with_window() {
        let base = compute_graph_layout(500.0, 12, Some(0.30));
        let doubled = compute_graph_layout(1000.0, 12, Some(0.30));
        assert!(doubled.column_width > base.column_width);
        assert!(doubled.lane_pitch > base.lane_pitch);
        assert!((doubled.column_width - 300.0).abs() <= 1.0);

        // Even with a large manual ratio on a narrow panel, commit info keeps its floor.
        let clamped = compute_graph_layout(320.0, 4, Some(0.90));
        assert!(320.0 - clamped.column_width >= GIT_GRAPH_MIN_COMMIT_WIDTH);
    }

    #[test]
    fn graph_layout_many_lanes_respects_minimum_pitch_and_reveals_more_when_widened() {
        let narrow = compute_graph_layout(380.0, 20, None);
        assert!(narrow.lane_pitch >= 14.0);
        assert!(narrow.canvas_width > narrow.column_width);

        let wide = compute_graph_layout(1200.0, 20, None);
        assert!(wide.lane_pitch >= 14.0);
        assert!(wide.column_width > narrow.column_width);
        assert!(wide.canvas_width <= wide.column_width);
    }

    #[test]
    fn group_commit_refs_combines_synced_remotes_without_false_positives() {
        let refs = vec![
            GitGraphRef {
                kind: GitGraphRefKind::Branch,
                name: "main".into(),
                current: true,
            },
            GitGraphRef {
                kind: GitGraphRefKind::Branch,
                name: "feat/prog-stim-sof-sync".into(),
                current: false,
            },
            GitGraphRef {
                kind: GitGraphRefKind::Branch,
                name: "stim".into(),
                current: false,
            },
            GitGraphRef {
                kind: GitGraphRefKind::Remote,
                name: "origin/main".into(),
                current: false,
            },
            GitGraphRef {
                kind: GitGraphRefKind::Remote,
                name: "origin/feat/prog-stim-sof-sync".into(),
                current: false,
            },
            GitGraphRef {
                kind: GitGraphRefKind::Remote,
                name: "origin/feat/stim".into(),
                current: false,
            },
            GitGraphRef {
                kind: GitGraphRefKind::Remote,
                name: "origin/other".into(),
                current: false,
            },
            GitGraphRef {
                kind: GitGraphRefKind::Tag,
                name: "v1.0.12".into(),
                current: false,
            },
        ];

        let grouped = group_commit_refs(&refs);
        assert_eq!(grouped.len(), 6);
        assert_eq!(grouped[0].primary.name, "main");
        assert_eq!(grouped[0].synced_remotes.len(), 1);
        assert_eq!(grouped[0].synced_remotes[0].name, "origin/main");

        assert_eq!(grouped[1].primary.name, "feat/prog-stim-sof-sync");
        assert_eq!(grouped[1].synced_remotes.len(), 1);
        assert_eq!(
            grouped[1].synced_remotes[0].name,
            "origin/feat/prog-stim-sof-sync"
        );

        // `origin/feat/stim` has a `/` in `origin/feat`, so it must NOT pair with `stim`.
        assert_eq!(grouped[2].primary.name, "stim");
        assert!(grouped[2].synced_remotes.is_empty());

        assert_eq!(grouped[3].primary.name, "origin/feat/stim");
        assert!(grouped[3].synced_remotes.is_empty());

        assert_eq!(grouped[4].primary.name, "origin/other");
        assert!(grouped[4].synced_remotes.is_empty());

        assert_eq!(grouped[5].primary.name, "v1.0.12");
        assert!(grouped[5].synced_remotes.is_empty());
    }
}
