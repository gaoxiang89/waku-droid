//! Low-frequency revision checks, independent of rendering and stream cadence.
//! The daemon uses native events; an unchanged poll does no scanning or redraw.

use super::*;
use waku_client::workspace::WorkspaceWatchState;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const ERROR_BACKOFF: Duration = Duration::from_secs(15);
const REVIEW_INTERVAL: Duration = Duration::from_secs(2);

pub(super) struct WorkspaceWatch {
    pub active: bool,
    client_id: Uuid,
    generation: u64,
    previous: Option<(PanelScope, WorkspaceWatchState)>,
    tree_pending: bool,
    content_pending: bool,
    review_pending: bool,
    last_review: Option<Instant>,
    warned: bool,
}

impl WorkspaceWatch {
    pub fn new(active: bool) -> Self {
        Self {
            active,
            client_id: Uuid::new_v4(),
            generation: 0,
            previous: None,
            tree_pending: false,
            content_pending: false,
            review_pending: false,
            last_review: None,
            warned: false,
        }
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.previous = None;
        self.tree_pending = false;
        self.content_pending = false;
        self.review_pending = false;
        self.last_review = None;
        self.warned = false;
    }
}

fn review_tracks_worktree(source: ReviewDiffSource) -> bool {
    matches!(
        source,
        ReviewDiffSource::Uncommitted | ReviewDiffSource::Unstaged | ReviewDiffSource::Branch
    )
}

fn review_changed(
    source: ReviewDiffSource,
    before: Option<&WorkspaceWatchState>,
    after: &WorkspaceWatchState,
) -> bool {
    if matches!(source, ReviewDiffSource::LastTurn { .. }) {
        return false;
    }
    let Some(before) = before.filter(|before| before.epoch == after.epoch) else {
        return true;
    };
    before.git != after.git || (review_tracks_worktree(source) && before.worktree != after.worktree)
}

impl Waku {
    fn watched_surfaces(&self) -> (bool, bool, bool) {
        if !self.workspace_watch.active {
            return (false, false, false);
        }
        let surface = self
            .right_panel_visible
            .then(|| self.active_right_panel_surface())
            .flatten();
        let files = self.files_panel_visible
            || matches!(
                surface,
                Some(RightPanelSurface::Files | RightPanelSurface::File(_))
            );
        let editor = matches!(surface, Some(RightPanelSurface::File(_)));
        let review = matches!(surface, Some(RightPanelSurface::Diff))
            && !matches!(
                self.right_panel_diff_source,
                ReviewDiffSource::LastTurn { .. }
            );
        (files, editor, review)
    }

    pub(super) fn start_workspace_watch(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut delay = POLL_INTERVAL;
            loop {
                cx.background_executor().timer(delay).await;
                let target = this.update(cx, |this, _| {
                    let (files, editor, review) = this.watched_surfaces();
                    if !files && !editor && !review {
                        return None;
                    }
                    let scope = this.selected_panel_scope()?;
                    let mut directories = Vec::new();
                    if files {
                        directories.extend(
                            this.right_panel_expanded_paths
                                .iter()
                                .filter_map(|path| {
                                    path.strip_prefix(&scope.workspace).ok().map(Path::to_owned)
                                })
                                .take(2048),
                        );
                    }
                    if editor && let Some(path) = this.visible_right_panel_file_path() {
                        directories.extend(Path::new(&path).parent().map(Path::to_owned));
                    }
                    directories.sort();
                    directories.dedup();
                    directories.truncate(2048);
                    let operation = waku_client::WorkspaceOperation::WatchChanges {
                        client_id: this.workspace_watch.client_id,
                        root: scope.workspace.clone(),
                        directories,
                        recursive: review && review_tracks_worktree(this.right_panel_diff_source),
                        watch_git: review,
                    };
                    Some((
                        scope,
                        this.workspace_watch.generation,
                        waku_client::WorkspaceClient::for_daemon(this.selected_daemon().clone()),
                        operation,
                    ))
                });
                let Ok(target) = target else {
                    break;
                };
                let Some((scope, generation, workspace, operation)) = target else {
                    delay = POLL_INTERVAL;
                    continue;
                };
                // One request in flight, including on slow SSH/WSL hosts.
                let result = cx
                    .background_executor()
                    .spawn(async move { workspace.request(operation) })
                    .await;
                delay = if result.is_ok() {
                    POLL_INTERVAL
                } else {
                    ERROR_BACKOFF
                };
                if this
                    .update(cx, |this, cx| {
                        if generation != this.workspace_watch.generation
                            || this.selected_panel_scope().as_ref() != Some(&scope)
                            || this.watched_surfaces() == (false, false, false)
                        {
                            return;
                        }
                        if let Ok(waku_client::WorkspaceResult::Changes { state }) = result {
                            this.apply_workspace_watch(scope, state, cx);
                        } else if !this.workspace_watch.warned {
                            this.workspace_watch.warned = true;
                            this.show_toast(tr!("files.auto_refresh_limited"));
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_workspace_watch(
        &mut self,
        scope: PanelScope,
        state: WorkspaceWatchState,
        cx: &mut Context<Self>,
    ) {
        if !state.ready {
            return;
        }
        let (files, editor, review) = self.watched_surfaces();
        if !files && !editor && !review {
            return;
        }
        let before = self
            .workspace_watch
            .previous
            .as_ref()
            .filter(|(previous_scope, previous)| {
                previous_scope == &scope && previous.epoch == state.epoch
            })
            .map(|(_, previous)| previous);
        self.workspace_watch.tree_pending |=
            files && before.is_none_or(|previous| previous.tree != state.tree);
        self.workspace_watch.content_pending |=
            editor && before.is_none_or(|previous| previous.content != state.content);
        self.workspace_watch.review_pending |=
            review && review_changed(self.right_panel_diff_source, before, &state);
        if state.limited && !self.workspace_watch.warned {
            self.workspace_watch.warned = true;
            self.show_toast(tr!("files.auto_refresh_limited"));
            cx.notify();
        }
        self.workspace_watch.previous = Some((scope, state));
        if files && self.workspace_watch.tree_pending && !self.right_panel_tree_refresh.is_loading()
        {
            self.workspace_watch.tree_pending = false;
            self.refresh_right_panel_working_tree(cx);
        }
        if editor
            && self.workspace_watch.content_pending
            && let Some(path) = self.visible_right_panel_file_path()
            && self
                .right_panel_file_editors
                .get(&path)
                .is_some_and(|editor| !editor.reading)
        {
            self.workspace_watch.content_pending = false;
            self.reload_right_panel_file_if_clean(&path, cx);
        }
        if review
            && self.workspace_watch.review_pending
            && !self.right_panel_diff_refresh.is_loading()
            && self
                .workspace_watch
                .last_review
                .is_none_or(|last| last.elapsed() >= REVIEW_INTERVAL)
        {
            self.workspace_watch.review_pending = false;
            self.workspace_watch.last_review = Some(Instant::now());
            self.refresh_right_panel_diff(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_only_refreshes_for_changes_in_its_source() {
        let before = WorkspaceWatchState {
            epoch: Uuid::new_v4(),
            tree: 1,
            content: 1,
            worktree: 1,
            git: 1,
            ready: true,
            limited: false,
        };
        let mut after = before.clone();
        after.tree += 1;
        after.content += 1; // An ignored build artifact must not recompute Review.
        assert!(!review_changed(
            ReviewDiffSource::Uncommitted,
            Some(&before),
            &after
        ));
        after.worktree += 1;
        assert!(review_changed(
            ReviewDiffSource::Uncommitted,
            Some(&before),
            &after
        ));
        assert!(!review_changed(
            ReviewDiffSource::Staged,
            Some(&before),
            &after
        ));
        after.git += 1;
        assert!(review_changed(
            ReviewDiffSource::Staged,
            Some(&before),
            &after
        ));
        assert!(!review_changed(
            ReviewDiffSource::LastTurn {
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                turn_count: 1
            },
            Some(&before),
            &after
        ));
    }

    #[test]
    fn reconnect_reconciles_even_when_counters_match() {
        let before = WorkspaceWatchState {
            epoch: Uuid::new_v4(),
            tree: 0,
            content: 0,
            worktree: 0,
            git: 0,
            ready: true,
            limited: false,
        };
        let mut after = before.clone();
        after.epoch = Uuid::new_v4();
        assert!(review_changed(
            ReviewDiffSource::Uncommitted,
            Some(&before),
            &after
        ));
    }
}
