//! Session notification refresh, dock badge, and read state.
use gpui::Context;
use hh_protocol::{ClientRequest, ServiceResponse};
use std::collections::HashSet;
use uuid::Uuid;

use crate::HhApp;
use crate::view_models::Modal;

#[cfg(target_os = "macos")]
pub(crate) fn set_macos_dock_badge(label: Option<&str>) {
    hh_macos_icon::set_dock_badge(label);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_macos_dock_badge(_: Option<&str>) {}

impl HhApp {
    pub(crate) fn refresh_notifications(&mut self) {
        self.dispatch_with(
            ClientRequest::GetNotifications,
            Box::new(|this, cx, result| {
                let previous = this.session.notifications.clone();
                match result {
                    Ok(ServiceResponse::Notifications { items }) => {
                        this.session.notifications_latest_id = items
                            .iter()
                            .map(|notification| notification.id)
                            .max()
                            .unwrap_or(0);
                        this.session.notifications = items;
                        this.session.connection_error = None;
                        this.sync_dock_badge();
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                if this.session.notifications != previous {
                    cx.notify();
                }
            }),
        );
    }

    pub(crate) fn unread_notification_count(&self) -> usize {
        self.session
            .notifications
            .iter()
            .filter(|notification| !notification.read)
            .count()
    }

    pub(crate) fn sync_dock_badge(&self) {
        let unread = self.unread_notification_count();
        if unread == 0 {
            set_macos_dock_badge(None);
        } else {
            let label = unread.to_string();
            set_macos_dock_badge(Some(&label));
        }
    }

    /// Marks the given notifications read locally (the visible state must
    /// change immediately) and persists the read state asynchronously.
    pub(crate) fn mark_notification_ids_read(&mut self, ids: &[u64]) -> bool {
        if ids.is_empty() {
            return false;
        }
        let id_set = ids.iter().copied().collect::<HashSet<_>>();
        let mut marked = false;
        for notification in &mut self.session.notifications {
            if id_set.contains(&notification.id) && !notification.read {
                notification.read = true;
                marked = true;
            }
        }
        if !marked {
            return false;
        }
        self.sync_dock_badge();
        self.dispatch_control(ClientRequest::MarkNotificationsRead { ids: ids.to_vec() });
        true
    }

    pub(crate) fn auto_read_pane_notifications(
        &mut self,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) -> bool {
        let ids = self
            .session
            .notifications
            .iter()
            .filter(|notification| notification.pane_id == pane_id && !notification.read)
            .map(|notification| notification.id)
            .collect::<Vec<_>>();
        let _ = cx;
        self.mark_notification_ids_read(&ids)
    }

    pub(crate) fn mark_all_notifications_read(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .session
            .notifications
            .iter()
            .filter(|notification| !notification.read)
            .map(|notification| notification.id)
            .collect::<Vec<_>>();
        if self.mark_notification_ids_read(&ids) {
            cx.notify();
        }
    }

    pub(crate) fn clear_notifications(&mut self, _cx: &mut Context<Self>) {
        self.dispatch_with(
            ClientRequest::ClearNotifications,
            Box::new(|this, cx, result| match result {
                Ok(ServiceResponse::Ack) => {
                    this.session.notifications.clear();
                    this.session.connection_error = None;
                    this.sync_dock_badge();
                    cx.notify();
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
    }

    pub(crate) fn open_notification(
        &mut self,
        notification_id: u64,
        pane_id: Uuid,
        workspace_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.mark_notification_ids_read(&[notification_id]);
        self.editor.modal = Modal::None;
        self.sidebar.active_workspace = Some(workspace_id);
        self.focus_pane_with_snapshot(pane_id, cx);
    }
}
