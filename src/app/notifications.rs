use super::*;
use gpui::EntityId;

/// The queue keeps this many notifications; older ones fall off the end.
const MAX_APP_NOTIFICATIONS: usize = 30;

/// Where an in-app notification points, so clicking it can jump there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AppNotificationTarget {
    Session(Uuid),
    Terminal(EntityId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AppNotificationKind {
    Completed,
    Failed,
    Waiting,
    Terminal,
}

impl AppNotificationKind {
    pub(super) fn icon_path(self) -> &'static str {
        match self {
            Self::Completed => "icons/check.svg",
            Self::Failed => "icons/x.svg",
            Self::Waiting => "icons/hourglass.svg",
            Self::Terminal => "icons/terminal-square.svg",
        }
    }

    pub(super) fn color(self, theme: Theme) -> Hsla {
        match self {
            Self::Completed => theme.success,
            Self::Failed => theme.danger,
            Self::Waiting => theme.warning,
            Self::Terminal => theme.text_secondary,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct AppNotification {
    pub(super) id: u64,
    pub(super) target: AppNotificationTarget,
    pub(super) kind: AppNotificationKind,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) created_at_ms: u64,
}

/// The in-app notification queue. Newest first; a pending notification for
/// the same target and kind is replaced rather than stacked, mirroring how
/// the system-notification tag supersedes an older banner for the same task.
#[derive(Default)]
pub(super) struct AppNotificationQueue {
    items: Vec<AppNotification>,
    next_id: u64,
}

impl AppNotificationQueue {
    pub(super) fn push(
        &mut self,
        target: AppNotificationTarget,
        kind: AppNotificationKind,
        title: String,
        body: String,
    ) {
        let created_at_ms = unix_time_millis();
        if let Some(index) = self
            .items
            .iter()
            .position(|notification| notification.target == target && notification.kind == kind)
        {
            let mut notification = self.items.remove(index);
            notification.title = title;
            notification.body = body;
            notification.created_at_ms = created_at_ms;
            self.items.insert(0, notification);
        } else {
            self.next_id += 1;
            self.items.insert(
                0,
                AppNotification {
                    id: self.next_id,
                    target,
                    kind,
                    title,
                    body,
                    created_at_ms,
                },
            );
            self.items.truncate(MAX_APP_NOTIFICATIONS);
        }
    }

    pub(super) fn dismiss(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|notification| notification.id != id);
        before != self.items.len()
    }

    pub(super) fn clear(&mut self) -> bool {
        if self.items.is_empty() {
            false
        } else {
            self.items.clear();
            true
        }
    }

    pub(super) fn find(&self, id: u64) -> Option<&AppNotification> {
        self.items.iter().find(|notification| notification.id == id)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &AppNotification> {
        self.items.iter()
    }
}

/// A row's render data, snapshotted during the toolbar's render like the
/// background-work entries before it, so the popover never borrows the entity
/// while it paints.
#[derive(Clone)]
pub(super) struct AppNotificationRow {
    pub(super) id: u64,
    pub(super) kind: AppNotificationKind,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) age: String,
    pub(super) focus: FocusHandle,
}

/// Compact age for a notification row, in the app's existing elapsed idiom
/// ("12s", "5m", "3h") rather than localized prose.
pub(super) fn notification_age(created_at_ms: u64) -> String {
    let seconds = unix_time_millis().saturating_sub(created_at_ms) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3_600)
    }
}

impl Waku {
    /// Queue an in-app notification.
    pub(super) fn push_app_notification(
        &mut self,
        target: AppNotificationTarget,
        kind: AppNotificationKind,
        title: String,
        body: String,
        cx: &mut Context<Self>,
    ) {
        self.app_notifications.push(target, kind, title, body);
        cx.notify();
    }

    pub(super) fn dismiss_app_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.app_notifications.dismiss(id) {
            cx.notify();
        }
    }

    pub(super) fn clear_app_notifications(&mut self, cx: &mut Context<Self>) {
        if self.app_notifications.clear() {
            cx.notify();
        }
    }

    /// Jump to a notification's target — the session it reported on, or the
    /// panel tab holding that terminal — and drop the notification.
    pub(super) fn open_app_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(notification) = self.app_notifications.find(id).cloned() else {
            return;
        };
        self.dismiss_app_notification(id, cx);
        match notification.target {
            AppNotificationTarget::Session(session_id) => self.select_session(session_id, cx),
            AppNotificationTarget::Terminal(terminal) => self.reveal_terminal(terminal, cx),
        }
    }

    /// Show the panel tab holding this terminal, in whichever panel owns it.
    fn reveal_terminal(&mut self, terminal: EntityId, cx: &mut Context<Self>) {
        if let Some(index) = self
            .bottom_panel_terminal_groups
            .iter()
            .position(|group| group.contains(terminal))
        {
            self.bottom_panel_active_group = Some(index);
            self.reveal_bottom_panel_tab(index);
            self.bottom_panel_pending_terminal_focus = true;
            self.set_bottom_panel_visible(true, cx);
            cx.notify();
            return;
        }
        if let Some(terminal_id) = self
            .right_panel_terminal_groups
            .iter()
            .find(|(_, group)| group.contains(terminal))
            .map(|(terminal_id, _)| *terminal_id)
        {
            self.open_right_panel_surface(RightPanelSurface::Terminal(terminal_id), cx);
        }
    }

    /// How many queued notifications still have a live target. The badge and
    /// tooltip read this every frame, so it stays an allocation-free scan.
    pub(super) fn app_notification_count(&self) -> usize {
        self.app_notifications
            .iter()
            .filter(|notification| self.notification_target_exists(notification))
            .count()
    }

    fn notification_target_exists(&self, notification: &AppNotification) -> bool {
        match notification.target {
            AppNotificationTarget::Session(session_id) => self
                .state
                .sessions
                .iter()
                .any(|session| session.id == session_id),
            AppNotificationTarget::Terminal(terminal) => {
                self.bottom_panel_terminal_groups
                    .iter()
                    .any(|group| group.contains(terminal))
                    || self
                        .right_panel_terminal_groups
                        .values()
                        .any(|group| group.contains(terminal))
            }
        }
    }

    /// Snapshot the queue for the popover, built only while it is open.
    /// Targets that no longer exist — a closed terminal tab, a removed
    /// session — are left out here rather than pruned from state, so the
    /// render path stays a read-only pass.
    pub(super) fn app_notification_rows(&self, cx: &mut App) -> Vec<AppNotificationRow> {
        self.app_notifications
            .iter()
            .filter(|notification| self.notification_target_exists(notification))
            .map(|notification| AppNotificationRow {
                id: notification.id,
                kind: notification.kind,
                title: notification.title.clone(),
                body: notification.body.clone(),
                age: notification_age(notification.created_at_ms),
                focus: self.transcript_control_focus(
                    format!("app-notification-row-{}", notification.id),
                    cx,
                ),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_with(
        target: AppNotificationTarget,
        kind: AppNotificationKind,
    ) -> AppNotificationQueue {
        let mut queue = AppNotificationQueue::default();
        queue.push(target, kind, "first".to_owned(), "body".to_owned());
        queue
    }

    #[test]
    fn same_target_and_kind_replaces_instead_of_stacking() {
        let session = Uuid::new_v4();
        let mut queue = queue_with(
            AppNotificationTarget::Session(session),
            AppNotificationKind::Completed,
        );
        queue.push(
            AppNotificationTarget::Session(session),
            AppNotificationKind::Completed,
            "second".to_owned(),
            "body".to_owned(),
        );
        let titles: Vec<_> = queue.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["second"]);

        // A different kind for the same target stacks alongside.
        queue.push(
            AppNotificationTarget::Session(session),
            AppNotificationKind::Waiting,
            "waiting".to_owned(),
            "body".to_owned(),
        );
        assert_eq!(queue.iter().count(), 2);
    }

    #[test]
    fn newest_notification_lands_first() {
        let mut queue = AppNotificationQueue::default();
        queue.push(
            AppNotificationTarget::Session(Uuid::new_v4()),
            AppNotificationKind::Completed,
            "older".to_owned(),
            "body".to_owned(),
        );
        queue.push(
            AppNotificationTarget::Session(Uuid::new_v4()),
            AppNotificationKind::Completed,
            "newer".to_owned(),
            "body".to_owned(),
        );
        assert_eq!(queue.iter().next().map(|n| n.title.as_str()), Some("newer"));
    }

    #[test]
    fn queue_is_bounded() {
        let mut queue = AppNotificationQueue::default();
        for _ in 0..(MAX_APP_NOTIFICATIONS + 5) {
            queue.push(
                AppNotificationTarget::Session(Uuid::new_v4()),
                AppNotificationKind::Completed,
                "title".to_owned(),
                "body".to_owned(),
            );
        }
        assert_eq!(queue.iter().count(), MAX_APP_NOTIFICATIONS);
    }

    #[test]
    fn dismiss_and_clear_report_whether_anything_changed() {
        let mut queue = queue_with(
            AppNotificationTarget::Terminal(gpui::EntityId::from(7u64)),
            AppNotificationKind::Terminal,
        );
        assert!(!queue.dismiss(999));
        let id = queue.iter().next().unwrap().id;
        assert!(queue.dismiss(id));
        assert!(!queue.clear());
        queue.push(
            AppNotificationTarget::Terminal(gpui::EntityId::from(8u64)),
            AppNotificationKind::Terminal,
            "again".to_owned(),
            "body".to_owned(),
        );
        assert!(queue.clear());
        assert_eq!(queue.iter().count(), 0);
    }
}
