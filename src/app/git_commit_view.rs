//! A commit is a page, not an accordion below the history list.
//! Both collections are virtualized; diff parsing stays on the background executor.
use super::git_graph_actions::{FILE_ROW_HEIGHT, InlineDiff};
use super::right_panel::{DiffRowStyle, render_diff_code_row};
use super::*;
use gpui::relative;
use waku_client::git::{GitCommitDetail, GitDiffSpec, GitGraphCommit};

/// The strip that titles the file list and the one that names the diff's
/// file, so both sections line up across the split.
const COMMIT_SECTION_HEADER_HEIGHT: f32 = 36.0;

pub(super) struct GitCommitView {
    pub(super) scope: Option<PanelScope>,
    pub(super) commit: Option<GitGraphCommit>,
    detail: Option<Arc<Result<GitCommitDetail, String>>>,
    selected: usize,
    files: ListState,
    files_scrollbar: Rc<ScrollbarState>,
    diff: Option<Arc<Result<InlineDiff, String>>>,
    lines: ListState,
    diff_scrollbar: Rc<ScrollbarState>,
    message_scroll: ScrollHandle,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(path: &str) -> PanelScope {
        PanelScope {
            host: HostKey::Local,
            workspace: PathBuf::from(path),
        }
    }

    fn commit(hash: &str) -> GitGraphCommit {
        GitGraphCommit {
            hash: hash.into(),
            parents: Vec::new(),
            author: "Author".into(),
            authored_at: 0,
            subject: "Subject".into(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn switching_commit_or_workspace_resets_file_selection() {
        let mut view = GitCommitView::new();
        view.open(scope("first"), commit("a"));
        view.selected = 3;
        view.open(scope("first"), commit("a"));
        assert_eq!(view.selected, 3);
        view.open(scope("first"), commit("b"));
        assert_eq!(view.selected, 0);
        view.selected = 2;
        view.open(scope("second"), commit("b"));
        assert_eq!(view.selected, 0);
        assert!(view.detail.is_none());
        assert!(view.diff.is_none());
    }

    #[test]
    fn returning_to_a_commit_starts_at_its_first_file() {
        let mut view = GitCommitView::new();
        view.open(scope("project"), commit("a"));
        view.selected = 4;
        view.scope = None;
        view.open(scope("project"), commit("a"));
        assert_eq!(view.selected, 0);
        assert_eq!(view.file_count(), 0);
    }
}

impl GitCommitView {
    pub(super) fn new() -> Self {
        Self {
            scope: None,
            commit: None,
            detail: None,
            selected: 0,
            files: ListState::new(0, ListAlignment::Top, px(180.0)),
            files_scrollbar: ScrollbarState::new(),
            diff: None,
            lines: ListState::new(0, ListAlignment::Top, px(512.0)),
            diff_scrollbar: ScrollbarState::new(),
            message_scroll: ScrollHandle::new(),
        }
    }

    pub(super) fn open(&mut self, scope: PanelScope, commit: GitGraphCommit) {
        if self.scope.as_ref() != Some(&scope)
            || self.commit.as_ref().map(|value| &value.hash) != Some(&commit.hash)
        {
            self.selected = 0;
            self.detail = None;
            self.diff = None;
            self.files.reset(0);
            self.lines.reset(0);
            self.message_scroll.set_offset(point(px(0.0), px(0.0)));
        }
        self.scope = Some(scope);
        self.commit = Some(commit);
    }

    fn file_count(&self) -> usize {
        self.detail
            .as_deref()
            .and_then(|value| value.as_ref().ok())
            .map_or(0, |detail| detail.files.len())
    }
}

impl Waku {
    pub(super) fn git_commit_page_open(&self) -> bool {
        self.git_commit_view.scope.is_some()
            && self.git_commit_view.scope == self.selected_panel_scope()
            && self.git_commit_view.commit.is_some()
    }

    pub(super) fn close_git_commit_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.git_commit_view.scope = None;
        self.git_graph_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn select_git_commit_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.git_commit_view.file_count() {
            return;
        }
        if index != self.git_commit_view.selected {
            self.git_commit_view.selected = index;
            self.git_commit_view.diff = None;
            self.git_commit_view.lines.reset(0);
            self.git_graph_ui.selection.clear();
        }
        self.git_commit_view.files.scroll_to_reveal_item(index);
        cx.notify();
    }

    pub(super) fn select_git_commit_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let index = self
            .git_commit_view
            .detail
            .as_deref()
            .and_then(|value| value.as_ref().ok())
            .and_then(|detail| detail.files.iter().position(|file| file.path == path));
        if let Some(index) = index {
            self.select_git_commit_file(index, cx);
        }
    }

    pub(super) fn step_git_commit_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.git_commit_view.file_count();
        if count > 0 {
            let index = self
                .git_commit_view
                .selected
                .saturating_add_signed(delta)
                .min(count - 1);
            self.select_git_commit_file(index, cx);
        }
    }

    pub(super) fn render_git_commit_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(commit) = self.git_commit_view.commit.clone() else {
            return div().into_any_element();
        };
        let Some(scope) = self.selected_panel_scope() else {
            return div().into_any_element();
        };
        let spec = GitDiffSpec::Commit {
            hash: commit.hash.clone(),
        };
        self.sync_git_graph_expanded(&scope, &spec);
        let detail = self.git_commit_detail(&commit.hash, cx);
        if let Some(detail) = &detail {
            if !self
                .git_commit_view
                .detail
                .as_ref()
                .is_some_and(|old| Arc::ptr_eq(old, detail))
            {
                let count = detail
                    .as_ref()
                    .as_ref()
                    .map_or(0, |detail| detail.files.len());
                self.git_commit_view
                    .files
                    .reset_with_uniform_height(count, px(FILE_ROW_HEIGHT));
                self.git_commit_view.selected =
                    self.git_commit_view.selected.min(count.saturating_sub(1));
                self.git_commit_view.detail = Some(detail.clone());
            }
        }
        let back_focus = self.transcript_control_focus("git-commit-back", cx);
        let back = div()
            .id("git-commit-back")
            .track_focus(&back_focus)
            .tab_index(0)
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(sp(12.5))
            .hover(|style| style.bg(theme.overlay))
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .child(icon("icons/arrow-left.svg", 14.0, theme.text_secondary))
            .child(tr!("git_graph.back_to_history"))
            .on_click(cx.listener(|this, _, window, cx| this.close_git_commit_page(window, cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !event.keystroke.modifiers.modified()
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.close_git_commit_page(window, cx);
                    cx.stop_propagation();
                }
            }));
        let metadata = div()
            .id("git-commit-metadata")
            .flex_none()
            .max_h(px(160.0))
            .overflow_y_scroll()
            .track_scroll(&self.git_commit_view.message_scroll)
            .px(px(14.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(5.0));
        let mut metadata = self.render_git_graph_commit_header(metadata, &commit, &theme, cx);
        if let Some(Ok(detail)) = detail.as_deref()
            && let Some(extra) = super::git_graph::message_body(&commit.subject, &detail.body)
        {
            metadata = metadata.child(
                div()
                    .text_size(sp(12.0))
                    .text_color(theme.text_secondary)
                    .whitespace_normal()
                    .child(extra.to_owned()),
            );
        }
        let content = match detail.as_deref() {
            None => self
                .render_right_panel_empty_message(tr!("git_graph.loading"), String::new(), cx)
                .into_any_element(),
            Some(Err(error)) => self
                .render_right_panel_empty_message(tr!("git_graph.error"), error.clone(), cx)
                .into_any_element(),
            Some(Ok(detail_value)) if detail_value.files.is_empty() => self
                .render_right_panel_empty_message(tr!("git_graph.no_files"), String::new(), cx)
                .into_any_element(),
            Some(Ok(detail_value)) => {
                let selected = self.git_commit_view.selected;
                let file = &detail_value.files[selected];
                let diff = self.git_graph_file_diff(&scope, &spec, file, cx);
                let diff_view = self.render_git_commit_diff(diff, &theme, cx);
                let entity = cx.entity().downgrade();
                let row_detail = detail.clone().unwrap();
                let file_list = list(self.git_commit_view.files.clone(), move |index, _, cx| {
                    entity
                        .upgrade()
                        .map(|entity| {
                            entity.update(cx, |this, cx| {
                                let theme = Theme::current(cx);
                                row_detail
                                    .as_ref()
                                    .as_ref()
                                    .ok()
                                    .and_then(|detail| detail.files.get(index))
                                    .map(|file| {
                                        this.render_git_graph_file_row(
                                            index,
                                            file,
                                            index == selected,
                                            false,
                                            &theme,
                                            cx,
                                        )
                                    })
                                    .unwrap_or_else(|| div().into_any_element())
                            })
                        })
                        .unwrap_or_else(|| div().into_any_element())
                })
                .size_full();
                let count = detail_value.files.len();
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .w(relative(0.32))
                            .max_w(px(280.0))
                            .min_w(px(110.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .min_h_0()
                            .border_r_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .h(px(COMMIT_SECTION_HEADER_HEIGHT))
                                    .flex_none()
                                    .px(px(10.0))
                                    .flex()
                                    .items_center()
                                    .text_size(sp(12.0))
                                    .text_color(theme.text_secondary)
                                    .child(tr!("git_graph.changed_files_count", count = count)),
                            )
                            .child(div().flex_1().min_h_0().relative().child(file_list).child(
                                scrollbar::vertical(
                                    &self.git_commit_view.files,
                                    &self.git_commit_view.files_scrollbar,
                                ),
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(COMMIT_SECTION_HEADER_HEIGHT))
                                    .flex_none()
                                    .px(px(12.0))
                                    .flex()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .child(
                                        div()
                                            .id("git-commit-file-path")
                                            .min_w_0()
                                            .truncate()
                                            .text_size(sp(12.0))
                                            .tooltip(Tooltip::text(file.path.clone()))
                                            .child(file.path.clone()),
                                    ),
                            )
                            .child(diff_view),
                    )
                    .into_any_element()
            }
        };
        div()
            .id("git-commit-page")
            .track_focus(&self.git_graph_focus)
            .tab_stop(false)
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(PANEL_TOOLBAR_HEIGHT))
                    .px(px(6.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(back),
            )
            .child(metadata)
            .child(content)
            .into_any_element()
    }

    fn render_git_commit_diff(
        &mut self,
        diff: Option<Arc<Result<InlineDiff, String>>>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(diff) = diff else {
            return self
                .render_right_panel_empty_message(tr!("git_graph.loading"), String::new(), cx)
                .into_any_element();
        };
        let value = match diff.as_ref() {
            Ok(value) => value,
            Err(error) => {
                return self
                    .render_right_panel_empty_message(tr!("git_graph.error"), error.clone(), cx)
                    .into_any_element();
            }
        };
        if value.lines.is_empty() {
            return self
                .render_right_panel_empty_message(
                    tr!("git_graph.no_textual_changes"),
                    String::new(),
                    cx,
                )
                .into_any_element();
        }
        if !self
            .git_commit_view
            .diff
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &diff))
        {
            self.git_commit_view.lines.reset(value.lines.len());
            self.git_commit_view.diff = Some(diff.clone());
            self.git_graph_ui.selection.clear();
        }
        let truncated = value.truncated;
        let selection = self.git_graph_ui.selection.clone();
        let input_selection = selection.clone();
        let style = DiffRowStyle::review(self.state.code_font_size);
        let rows = list(self.git_commit_view.lines.clone(), move |index, _, cx| {
            let theme = Theme::current(cx);
            let Some(line) = diff
                .as_ref()
                .as_ref()
                .ok()
                .and_then(|diff| diff.lines.get(index))
            else {
                return div().into_any_element();
            };
            match &line.kind {
                crate::review_diff::LineKind::Context
                | crate::review_diff::LineKind::Addition
                | crate::review_diff::LineKind::Deletion => {
                    render_diff_code_row(line, index, "git-commit-diff", &selection, style, &theme)
                }
                crate::review_diff::LineKind::Gap(gap) => div()
                    .px(px(10.0))
                    .py(px(5.0))
                    .text_size(sp(12.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("diff.unmodified_lines", count = gap.count()))
                    .into_any_element(),
                _ => div()
                    .px(px(10.0))
                    .py(px(5.0))
                    .text_size(sp(12.0))
                    .text_color(theme.text_secondary)
                    .child(line.content.clone())
                    .into_any_element(),
            }
        })
        .size_full();
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .child(md::render::frame_reset(self.git_graph_ui.selection.clone()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .font_family(md::render::MONO_FAMILY)
                    .text_size(px(self.state.code_font_size))
                    .child(rows)
                    .child(scrollbar::vertical(
                        &self.git_commit_view.lines,
                        &self.git_commit_view.diff_scrollbar,
                    )),
            )
            .when(truncated, |element| {
                element.child(
                    div()
                        .px(px(10.0))
                        .py(px(5.0))
                        .text_size(sp(12.0))
                        .text_color(theme.text_tertiary)
                        .child(tr!("git_graph.diff_truncated")),
                )
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        md::render::install_selection_input(window, &input_selection)
                    },
                )
                .absolute()
                .w(px(0.0))
                .h(px(0.0)),
            )
            .into_any_element()
    }
}
