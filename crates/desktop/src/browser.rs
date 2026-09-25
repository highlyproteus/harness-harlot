//! Embedded browser panes: CEF wiring, URL editing, and navigation.
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use anyhow::Context as _;
#[cfg(all(target_os = "macos", feature = "browser"))]
use gpui::Window;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use gpui::canvas;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use gpui::{AppContext, Image, ImageFormat};
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use hh_protocol::BrowserAction;
use hh_protocol::{
    BrowserCommandOutcome, BrowserCommandRequest, ClientRequest, DropPlacement, Pane, PaneKind,
    ServiceResponse,
};
#[cfg(all(target_os = "macos", feature = "browser"))]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use std::cell::RefCell;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use std::collections::{HashMap, HashSet};
#[cfg(all(target_os = "macos", feature = "browser"))]
use std::ffi::c_void;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use std::rc::Rc;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use std::sync::Arc;
#[cfg(all(target_os = "macos", feature = "browser"))]
use std::sync::LazyLock;
#[cfg(all(target_os = "linux", feature = "browser"))]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use crate::PANE_HEADER_HEIGHT;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use crate::helpers::split_placement_at;
use crate::helpers::{
    collect_terminal_tabs, element_key, find_pane, workspace_tab_standalone_pane,
};
use crate::input::browser_url_editor_is_active;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use crate::session::session_call;
use crate::view_models::Modal;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use crate::view_models::{DragDestination, PaneDrag, TabDrag};
use crate::{HhApp, THEME};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct BrowserUrlEditor {
    pub(crate) pane_id: Uuid,
    pub(crate) text: String,
    pub(crate) replace_on_type: bool,
    pub(crate) invalid: bool,
}

#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub(crate) struct BrowserShared {
    pub(crate) url: String,
    pub(crate) title: Option<String>,
    pub(crate) favicon: Option<Arc<Image>>,
    pub(crate) loading: bool,
    pub(crate) can_go_back: bool,
    pub(crate) can_go_forward: bool,
    pub(crate) dirty: bool,
    pub(crate) popup_requests: Vec<String>,
    pub(crate) focus_requested: bool,
    pub(crate) dev_tools_results: Vec<(i32, bool, Vec<u8>)>,
}

#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
#[derive(Debug)]
pub(crate) struct BrowserPaneView {
    pub(crate) pane: Rc<hh_cef_view::BrowserPane>,
    pub(crate) shared: Rc<RefCell<BrowserShared>>,
    pub(crate) last_snapshot_url: String,
    pub(crate) synced_url: String,
    pub(crate) synced_title: Option<String>,
    pub(crate) pending_state: Option<(String, Option<String>)>,
    pub(crate) in_flight_state: Option<(String, Option<String>)>,
    pub(crate) focused: bool,
    pub(crate) visible: bool,
    pub(crate) bounds: Rc<RefCell<Option<(hh_cef_view::BrowserRect, f32)>>>,
    pub(crate) cdp_pending: HashMap<i32, u64>,
    pub(crate) next_cdp_id: i32,
}

#[cfg(all(target_os = "macos", feature = "browser"))]
static BROWSER_COMMAND_AVAILABLE: LazyLock<bool> = LazyLock::new(hh_cef_view::preflight);

#[cfg(all(target_os = "linux", feature = "browser"))]
static BROWSER_X11_AVAILABLE: AtomicBool = AtomicBool::new(false);

#[cfg(all(target_os = "macos", feature = "browser"))]
pub(crate) fn browser_command_available() -> bool {
    *BROWSER_COMMAND_AVAILABLE
}

#[cfg(all(target_os = "linux", feature = "browser"))]
pub(crate) fn browser_command_available() -> bool {
    BROWSER_X11_AVAILABLE.load(Ordering::Acquire)
        && hh_cef_view::preflight()
        && hh_cef_view::sandbox_available()
}

#[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
pub(crate) fn browser_command_available() -> bool {
    false
}

#[cfg(all(target_os = "linux", feature = "browser"))]
pub(crate) fn configure_linux_browser_backend() {
    let x11_available = std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty());
    BROWSER_X11_AVAILABLE.store(x11_available, Ordering::Release);
    if x11_available {
        // SAFETY: This runs at process entry, before GPUI starts worker threads
        // or reads the compositor environment.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("WAYLAND_DISPLAY");
        }
    }
}

#[cfg(all(target_os = "macos", feature = "browser"))]
pub(crate) fn browser_unavailable_reason() -> &'static str {
    "Browser tabs require the bundled macOS app"
}

#[cfg(all(target_os = "linux", feature = "browser"))]
pub(crate) fn browser_unavailable_reason() -> &'static str {
    if !BROWSER_X11_AVAILABLE.load(Ordering::Acquire) {
        "Browser tabs require an X11 or XWayland session"
    } else if !hh_cef_view::sandbox_available() {
        "Browser tabs require unprivileged user namespaces; this kernel restricts them"
    } else {
        "Browser tabs require the packaged Linux install"
    }
}

#[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
pub(crate) fn browser_unavailable_reason() -> &'static str {
    "Browser tabs require the bundled macOS app"
}

#[cfg(all(target_os = "macos", feature = "browser"))]
pub(crate) fn native_nsview(window: &Window) -> Option<*mut c_void> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(handle) => Some(handle.ns_view.as_ptr()),
        _ => None,
    }
}

#[cfg(all(target_os = "macos", feature = "browser"))]
pub(crate) fn prepare_cef_process() {
    if !hh_cef_view::preflight() {
        return;
    }
    if let Some(exit_code) = hh_cef_view::early_process_split() {
        std::process::exit(exit_code);
    }
    hh_cef_view::install_nsapp_protocol();
}

#[cfg(all(target_os = "linux", feature = "browser"))]
pub(crate) fn prepare_cef_process() {
    let _ = hh_cef_view::preflight();
}

#[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
pub(crate) fn prepare_cef_process() {}

impl HhApp {
    pub(crate) fn new_browser_tab(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.active_workstation() else {
            return;
        };
        self.new_browser_tab_in(workspace_id, cx);
    }

    pub(crate) fn browser_group_target(&self, workspace_id: Uuid) -> Option<Uuid> {
        let workspace = self
            .session
            .snapshot
            .as_ref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)?;
        self.layout
            .focused_pane
            .filter(|pane_id| {
                workspace.tabs.iter().any(|tab| {
                    find_pane(&tab.layout, *pane_id).is_some_and(|pane| pane.kind.is_terminal())
                })
            })
            .or_else(|| {
                workspace
                    .tabs
                    .iter()
                    .flat_map(|tab| {
                        let mut panes = Vec::new();
                        collect_terminal_tabs(&tab.layout, &mut panes);
                        panes
                    })
                    .find(|pane| pane.kind.is_terminal())
                    .map(|pane| pane.id)
            })
    }

    pub(crate) fn create_browser(
        &mut self,
        workspace_id: Uuid,
        request: ClientRequest,
        cx: &mut Context<Self>,
    ) {
        if !browser_command_available() {
            self.session.connection_error = Some(browser_unavailable_reason().to_owned());
            self.editor.modal = Modal::None;
            cx.notify();
            return;
        }
        self.dispatch_with(
            request,
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::PaneCreated { pane_id }) => {
                        this.sidebar.active_workspace = Some(workspace_id);
                        this.focus_pane_with_snapshot(pane_id, cx);
                        this.sidebar.expanded_workspaces.insert(workspace_id);
                        this.editor.browser_url_editor = Some(BrowserUrlEditor {
                            pane_id,
                            text: String::new(),
                            replace_on_type: true,
                            invalid: false,
                        });
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                this.layout.last_sizes.clear();
                this.editor.modal = Modal::None;
                cx.notify();
            }),
        );
    }

    /// Opens a terminal-detected URL as an embedded browser pane split to the
    /// right of the terminal it came from. Falls back to the default browser
    /// when the embedded browser build is unavailable.
    pub(crate) fn open_url_in_browser_split(
        &mut self,
        target_pane: Uuid,
        url: &str,
        cx: &mut Context<Self>,
    ) {
        if !browser_command_available() {
            cx.open_url(url);
            cx.notify();
            return;
        }
        self.dispatch_with(
            ClientRequest::CreateGroupBrowser {
                target_pane,
                url: Some(url.to_owned()),
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::PaneCreated {
                        pane_id: browser_pane,
                    }) => {
                        this.move_pane_to_split(
                            browser_pane,
                            target_pane,
                            DropPlacement::Right,
                            cx,
                        );
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    pub(crate) fn new_workspace_browser(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.create_browser(
            workspace_id,
            ClientRequest::CreateBrowserTab {
                workspace_id,
                url: None,
            },
            cx,
        );
    }

    pub(crate) fn new_browser_tab_in(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let request = self.browser_group_target(workspace_id).map_or(
            ClientRequest::CreateBrowserTab {
                workspace_id,
                url: None,
            },
            |target_pane| ClientRequest::CreateGroupBrowser {
                target_pane,
                url: None,
            },
        );
        self.create_browser(workspace_id, request, cx);
    }

    pub(crate) fn tab_is_navigation_container(&self, tab_id: Uuid) -> bool {
        self.session.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .is_some_and(|tab| workspace_tab_standalone_pane(tab).is_none())
        })
    }

    pub(crate) fn project_for_create_target(&self, tab_id: Uuid) -> Option<Uuid> {
        self.session.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| {
                    if tab.project_dir.is_some() {
                        Some(tab.id)
                    } else {
                        tab.parent_tab
                    }
                })
        })
    }

    pub(crate) fn add_terminal_to_context(
        &mut self,
        workspace_id: Uuid,
        target_tab: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab_id) = target_tab.filter(|tab_id| self.tab_is_navigation_container(*tab_id))
        {
            self.new_group_terminal(tab_id, cx);
        } else {
            self.new_workspace_tab(workspace_id, cx);
        }
    }

    pub(crate) fn add_browser_to_context(
        &mut self,
        workspace_id: Uuid,
        target_tab: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab_id) = target_tab.filter(|tab_id| self.tab_is_navigation_container(*tab_id))
        {
            self.new_group_browser(tab_id, cx);
        } else {
            self.new_workspace_browser(workspace_id, cx);
        }
    }

    pub(crate) fn add_group_to_context(
        &mut self,
        workspace_id: Uuid,
        target_tab: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if let Some(project_id) =
            target_tab.and_then(|tab_id| self.project_for_create_target(tab_id))
        {
            self.new_project_group(workspace_id, project_id, cx);
        } else {
            self.new_workspace_group(workspace_id, cx);
        }
    }

    pub(crate) fn begin_browser_url_edit(
        &mut self,
        pane_id: Uuid,
        url: &str,
        cx: &mut Context<Self>,
    ) {
        self.editor.browser_url_editor = Some(BrowserUrlEditor {
            pane_id,
            text: url.to_owned(),
            replace_on_type: true,
            invalid: false,
        });
        cx.notify();
    }

    pub(crate) fn append_browser_url_text(&mut self, text: &str) -> bool {
        if !browser_url_editor_is_active(
            self.editor
                .browser_url_editor
                .as_ref()
                .map(|editor| editor.pane_id),
            self.layout.focused_pane,
        ) {
            return false;
        }
        let Some(editor) = self.editor.browser_url_editor.as_mut() else {
            return false;
        };
        if editor.replace_on_type {
            editor.text.clear();
        }
        for character in text.chars().filter(|character| !character.is_control()) {
            if editor.text.len() + character.len_utf8() > hh_protocol::MAX_BROWSER_URL_LEN {
                break;
            }
            editor.text.push(character);
        }
        editor.replace_on_type = false;
        editor.invalid = false;
        true
    }

    pub(crate) fn submit_browser_url(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.browser_url_editor.clone() else {
            return;
        };
        let Ok(url) = hh_protocol::normalize_browser_url(&editor.text) else {
            if let Some(editor) = self.editor.browser_url_editor.as_mut() {
                editor.invalid = true;
            }
            cx.notify();
            return;
        };
        let pane_id = editor.pane_id;
        self.dispatch_with(
            ClientRequest::SetBrowserState {
                pane_id,
                url: url.clone(),
                title: None,
            },
            Box::new(move |this, cx, result| match result {
                Ok(ServiceResponse::Ack) => {
                    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
                    if let Some(view) = this.browser.browser_views.get(&pane_id) {
                        view.pane.navigate(&url);
                    }
                    this.editor.browser_url_editor = None;
                    this.session.connection_error = None;
                    cx.notify();
                }
                Ok(response) => this.report_unexpected(&response),
                Err(error) => this.report(&error),
            }),
        );
    }
    pub(crate) fn browser_back(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        if let Some(view) = self.browser.browser_views.get(&pane_id) {
            view.pane.back();
        }
        #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
        let _ = pane_id;
        cx.notify();
    }

    pub(crate) fn browser_forward(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        if let Some(view) = self.browser.browser_views.get(&pane_id) {
            view.pane.forward();
        }
        #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
        let _ = pane_id;
        cx.notify();
    }

    pub(crate) fn browser_reload(&mut self, pane_id: Uuid, loading: bool, cx: &mut Context<Self>) {
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        if let Some(view) = self.browser.browser_views.get(&pane_id) {
            if loading {
                view.pane.stop();
            } else {
                view.pane.reload();
            }
        }
        #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
        let _ = (pane_id, loading);
        cx.notify();
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn visible_browser_panes(&self) -> HashSet<Uuid> {
        let Some(workspace) = self
            .session
            .snapshot
            .as_ref()
            .and_then(|snapshot| self.active_workspace_in(snapshot))
        else {
            return HashSet::new();
        };
        let Some(layout) =
            crate::helpers::workspace_layout_for_focused_pane(workspace, self.layout.focused_pane)
        else {
            return HashSet::new();
        };
        if matches!(self.editor.modal, Modal::AppearanceSettings) {
            return HashSet::new();
        }
        if let Some(pane) = self.layout.zoomed_pane.and_then(|id| find_pane(layout, id)) {
            return pane
                .kind
                .is_browser()
                .then_some(pane.id)
                .into_iter()
                .collect();
        }
        crate::helpers::visible_panes(layout)
            .into_iter()
            .filter(|id| find_pane(layout, *id).is_some_and(|pane| pane.kind.is_browser()))
            .collect()
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn ensure_visible_browser_views(&mut self, cx: &mut Context<Self>) {
        for pane_id in self.visible_browser_panes() {
            let Some(Pane {
                kind: PaneKind::Browser { url },
                ..
            }) = self.pane_metadata(pane_id)
            else {
                continue;
            };
            let (width, height) = self.layout.workspace_pixels;
            if let Err(error) = self.ensure_browser_view(pane_id, &url, width, height, cx) {
                let error = format!("{error:#}");
                if self.browser.browser_runtime_error.as_deref() != Some(&error) {
                    self.browser.browser_runtime_error = Some(error);
                    cx.notify();
                }
            }
        }
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn schedule_browser_presentation(
        &self,
        window: &mut gpui::Window,
        cx: &Context<Self>,
    ) {
        let visible = self.visible_browser_panes();
        let content_visible =
            matches!(self.editor.modal, Modal::None) && self.layout.dragging_pane.is_none();
        let should_focus = content_visible
            && self.session.window_active
            && !browser_url_editor_is_active(
                self.editor
                    .browser_url_editor
                    .as_ref()
                    .map(|editor| editor.pane_id),
                self.layout.focused_pane,
            );
        let changed = self.browser.browser_views.iter().any(|(id, view)| {
            let visible_now = content_visible && visible.contains(id);
            let focused_now = visible_now && should_focus && Some(*id) == self.layout.focused_pane;
            view.visible != visible_now || view.focused != focused_now
        });
        if changed || self.browser.reassert_focus {
            cx.on_next_frame(window, move |this, _, _| {
                let visible = this.visible_browser_panes();
                this.sync_browser_view_presentation(&visible);
            });
        }
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn ensure_browser_view(
        &mut self,
        pane_id: Uuid,
        url: &str,
        width: f32,
        height: f32,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        #[cfg(target_os = "macos")]
        let _ = cx;
        if !self.browser.browser_runtime_initialized {
            let cache_dir = hh_protocol::state_directory()
                .ok_or_else(|| anyhow::anyhow!("app state directory is unavailable"))?
                .join("browser-cache");
            std::fs::create_dir_all(&cache_dir)
                .context("create Chromium browser cache directory")?;
            hh_cef_view::init_runtime(&cache_dir)?;
            self.browser.browser_runtime_initialized = true;
            self.browser.browser_runtime_error = None;
            #[cfg(target_os = "linux")]
            cx.spawn(async move |this, cx| {
                loop {
                    gpui::Timer::after(std::time::Duration::from_millis(10)).await;
                    let keep_pumping = this
                        .update(cx, |this, _| {
                            if this.browser.browser_runtime_initialized {
                                hh_cef_view::pump_runtime();
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    if !keep_pumping {
                        break;
                    }
                }
            })
            .detach();
        }
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.browser.browser_views.entry(pane_id)
        {
            #[cfg(target_os = "macos")]
            let parent_view = self
                .browser
                .browser_parent_view
                .filter(|view| !view.is_null())
                .ok_or_else(|| anyhow::anyhow!("GPUI did not expose a parent NSView"))?;
            let shared = Rc::new(RefCell::new(BrowserShared {
                url: url.to_owned(),
                title: None,
                favicon: None,
                loading: true,
                can_go_back: false,
                can_go_forward: false,
                dirty: false,
                popup_requests: Vec::new(),
                focus_requested: false,
                dev_tools_results: Vec::new(),
            }));
            let address_state = Rc::clone(&shared);
            let title_state = Rc::clone(&shared);
            let favicon_state = Rc::clone(&shared);
            let loading_state = Rc::clone(&shared);
            let popup_state = Rc::clone(&shared);
            let focus_state = Rc::clone(&shared);
            let dev_tools_state = Rc::clone(&shared);
            let callbacks = hh_cef_view::Callbacks {
                on_got_focus: Box::new(move || {
                    let mut state = focus_state.borrow_mut();
                    state.focus_requested = true;
                    state.dirty = true;
                }),
                on_popup_request: Box::new(move |url| {
                    let mut state = popup_state.borrow_mut();
                    if state.popup_requests.len() < 4 {
                        state.popup_requests.push(url);
                        state.dirty = true;
                    }
                }),
                on_address_change: Box::new(move |next_url| {
                    let mut state = address_state.borrow_mut();
                    if state.url != next_url {
                        state.url = next_url;
                        state.dirty = true;
                    }
                }),
                on_title_change: Box::new(move |next_title| {
                    let mut state = title_state.borrow_mut();
                    let next_title = (!next_title.trim().is_empty()).then_some(next_title);
                    if state.title != next_title {
                        state.title = next_title;
                        state.dirty = true;
                    }
                }),
                on_favicon_change: Box::new(move |favicon| {
                    let mut state = favicon_state.borrow_mut();
                    state.favicon =
                        favicon.map(|png| Arc::new(Image::from_bytes(ImageFormat::Png, png)));
                    state.dirty = true;
                }),
                on_loading_state: Box::new(move |loading, can_go_back, can_go_forward| {
                    let mut state = loading_state.borrow_mut();
                    if state.loading != loading
                        || state.can_go_back != can_go_back
                        || state.can_go_forward != can_go_forward
                    {
                        state.loading = loading;
                        state.can_go_back = can_go_back;
                        state.can_go_forward = can_go_forward;
                        state.dirty = true;
                    }
                }),
                on_dev_tools_result: Box::new(move |message_id, success, result| {
                    let mut state = dev_tools_state.borrow_mut();
                    state.dev_tools_results.push((message_id, success, result));
                    state.dirty = true;
                }),
            };
            let rect = hh_cef_view::BrowserRect {
                x: 0.0,
                y: 0.0,
                width: width.max(1.0),
                height: (height - PANE_HEADER_HEIGHT - 38.0).max(1.0),
            };
            #[cfg(target_os = "macos")]
            let browser = Rc::new(hh_cef_view::BrowserPane::create(
                parent_view,
                rect,
                url,
                callbacks,
            )?);
            #[cfg(target_os = "linux")]
            let browser = Rc::new(hh_cef_view::BrowserPane::create(rect, url, callbacks)?);
            entry.insert(BrowserPaneView {
                pane: browser,
                shared,
                last_snapshot_url: url.to_owned(),
                synced_url: url.to_owned(),
                synced_title: None,
                focused: false,
                visible: true,
                bounds: Rc::new(RefCell::new(None)),
                pending_state: None,
                in_flight_state: None,
                cdp_pending: HashMap::new(),
                next_cdp_id: 1,
            });
            cx.notify();
        }
        if let Some(view) = self.browser.browser_views.get_mut(&pane_id) {
            // Renderer navigation is persisted back through the snapshot. That
            // echo must not reload the document (notably after history.pushState).
            let is_navigation_echo = view.shared.borrow().url == url
                || view.synced_url == url
                || view
                    .in_flight_state
                    .as_ref()
                    .is_some_and(|(submitted_url, _)| submitted_url == url);
            if view.last_snapshot_url != url && !is_navigation_echo {
                view.pane.navigate(url);
                url.clone_into(&mut view.synced_url);
            }
            url.clone_into(&mut view.last_snapshot_url);
        }
        Ok(())
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn sync_browser_view_presentation(&mut self, visible: &HashSet<Uuid>) {
        let content_visible =
            matches!(self.editor.modal, Modal::None) && self.layout.dragging_pane.is_none();
        let should_focus = content_visible
            && self.session.window_active
            && !browser_url_editor_is_active(
                self.editor
                    .browser_url_editor
                    .as_ref()
                    .map(|editor| editor.pane_id),
                self.layout.focused_pane,
            );
        let reassert = std::mem::take(&mut self.browser.reassert_focus);
        let mut blurred = false;
        for (view_id, view) in &mut self.browser.browser_views {
            let visible_now = content_visible && visible.contains(view_id);
            let focused_now =
                visible_now && should_focus && Some(*view_id) == self.layout.focused_pane;
            view.pane.set_visible(visible_now);
            view.visible = visible_now;
            if !focused_now && (view.focused || reassert) {
                view.pane.focus(false);
                view.focused = false;
                blurred = true;
            }
        }
        // Blur every old native responder before focusing the new one: HashMap
        // iteration order must never hand focus back to GPUI after this step.
        if should_focus
            && let Some(pane_id) = self.layout.focused_pane.filter(|id| visible.contains(id))
            && let Some(view) = self.browser.browser_views.get_mut(&pane_id)
            && (!view.focused || blurred || reassert)
        {
            view.pane.focus(true);
            view.focused = true;
        }
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn sync_browser_callback_state(&mut self, cx: &mut Context<Self>) -> bool {
        let browser_ids = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .flat_map(|workspace| &workspace.tabs)
                    .flat_map(|tab| {
                        let mut panes = Vec::new();
                        collect_terminal_tabs(&tab.layout, &mut panes);
                        panes
                    })
                    .filter_map(|pane| pane.kind.is_browser().then_some(pane.id))
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        self.browser
            .browser_views
            .retain(|pane_id, _| browser_ids.contains(pane_id));

        let updates = self
            .browser
            .browser_views
            .iter()
            .filter_map(|(pane_id, view)| {
                let mut shared = view.shared.borrow_mut();
                if !shared.dirty {
                    return None;
                }
                shared.dirty = false;
                Some((
                    *pane_id,
                    shared.url.clone(),
                    shared.title.clone(),
                    std::mem::take(&mut shared.popup_requests),
                    std::mem::take(&mut shared.focus_requested),
                ))
            })
            .collect::<Vec<_>>();
        let changed = !updates.is_empty();
        for (pane_id, url, title, popups, focus_requested) in updates {
            let Some(view) = self.browser.browser_views.get_mut(&pane_id) else {
                continue;
            };
            let next = (url, title);
            if let Some(in_flight) = view.in_flight_state.as_ref() {
                view.pending_state = (in_flight != &next).then_some(next);
            } else if view.synced_url == next.0 && view.synced_title == next.1 {
                view.pending_state = None;
            } else {
                view.pending_state = Some(next);
            }
            if focus_requested {
                view.focused = true;
                if self.layout.focused_pane != Some(pane_id) {
                    self.focus_pane_with_snapshot(pane_id, cx);
                }
            }
            for url in popups {
                self.open_url_in_browser_split(pane_id, &url, cx);
            }
        }
        changed
    }

    fn post_browser_command_result(&mut self, request_id: u64, outcome: BrowserCommandOutcome) {
        self.dispatch(ClientRequest::BrowserCommandResult {
            request_id,
            outcome,
        });
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    fn execute_browser_command(
        &mut self,
        command: BrowserCommandRequest,
        deadline: std::time::Instant,
        cx: &mut Context<Self>,
    ) {
        let BrowserCommandRequest {
            request_id,
            pane_id,
            action,
        } = command;
        let Some(view) = self.browser.browser_views.get_mut(&pane_id) else {
            if std::time::Instant::now() >= deadline {
                self.post_browser_command_result(
                    request_id,
                    BrowserCommandOutcome::Error {
                        message: "browser pane did not become ready".to_owned(),
                    },
                );
            } else {
                self.focus_pane_with_snapshot(pane_id, cx);
                self.browser.deferred_commands.push((
                    BrowserCommandRequest {
                        request_id,
                        pane_id,
                        action,
                    },
                    deadline,
                ));
            }
            return;
        };
        if matches!(&action, BrowserAction::DevTools { .. }) && !view.pane.is_created() {
            if std::time::Instant::now() >= deadline {
                self.post_browser_command_result(
                    request_id,
                    BrowserCommandOutcome::Error {
                        message: "browser pane did not become ready".to_owned(),
                    },
                );
            } else {
                self.browser.deferred_commands.push((
                    BrowserCommandRequest {
                        request_id,
                        pane_id,
                        action,
                    },
                    deadline,
                ));
            }
            return;
        }
        let outcome = match action {
            BrowserAction::Navigate { url } => {
                view.pane.navigate(&url);
                Some(BrowserCommandOutcome::Ok {
                    result: serde_json::Value::Null,
                })
            }
            BrowserAction::Back => {
                view.pane.back();
                Some(BrowserCommandOutcome::Ok {
                    result: serde_json::Value::Null,
                })
            }
            BrowserAction::Forward => {
                view.pane.forward();
                Some(BrowserCommandOutcome::Ok {
                    result: serde_json::Value::Null,
                })
            }
            BrowserAction::Reload => {
                view.pane.reload();
                Some(BrowserCommandOutcome::Ok {
                    result: serde_json::Value::Null,
                })
            }
            BrowserAction::DevTools { method, params } => {
                let message_id = view.next_cdp_id;
                view.next_cdp_id = view.next_cdp_id.checked_add(1).unwrap_or(1);
                match serde_json::to_vec(&serde_json::json!({
                    "id": message_id,
                    "method": method,
                    "params": params,
                })) {
                    Ok(message) => {
                        view.cdp_pending.insert(message_id, request_id);
                        if view.pane.send_dev_tools_message(&message) {
                            None
                        } else {
                            view.cdp_pending.remove(&message_id);
                            Some(BrowserCommandOutcome::Error {
                                message: "browser is not ready".to_owned(),
                            })
                        }
                    }
                    Err(error) => Some(BrowserCommandOutcome::Error {
                        message: format!("could not encode DevTools call: {error}"),
                    }),
                }
            }
        };
        if let Some(outcome) = outcome {
            self.post_browser_command_result(request_id, outcome);
        }
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn run_browser_commands(
        &mut self,
        commands: Vec<BrowserCommandRequest>,
        cx: &mut Context<Self>,
    ) {
        for command in commands {
            self.execute_browser_command(
                command,
                std::time::Instant::now() + std::time::Duration::from_secs(10),
                cx,
            );
        }
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn poll_browser_commands(&mut self, cx: &mut Context<Self>) {
        let deferred = std::mem::take(&mut self.browser.deferred_commands);
        for (command, deadline) in deferred {
            self.execute_browser_command(command, deadline, cx);
        }
        let mut completed = Vec::new();
        for view in self.browser.browser_views.values_mut() {
            let results = {
                let mut shared = view.shared.borrow_mut();
                std::mem::take(&mut shared.dev_tools_results)
            };
            for (message_id, success, bytes) in results {
                let Some(request_id) = view.cdp_pending.remove(&message_id) else {
                    continue;
                };
                let outcome = if bytes.len() > 3 * 1024 * 1024 {
                    BrowserCommandOutcome::Error {
                        message: "result exceeds 3 MiB".to_owned(),
                    }
                } else {
                    match serde_json::from_slice::<serde_json::Value>(&bytes) {
                        Ok(result) if success && result.get("error").is_none() => {
                            BrowserCommandOutcome::Ok { result }
                        }
                        Ok(result) => BrowserCommandOutcome::Error {
                            message: result
                                .pointer("/error/message")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("DevTools call failed")
                                .to_owned(),
                        },
                        Err(_) => BrowserCommandOutcome::Error {
                            message: "DevTools call failed".to_owned(),
                        },
                    }
                };
                completed.push((request_id, outcome));
            }
        }
        for (request_id, outcome) in completed {
            self.post_browser_command_result(request_id, outcome);
        }
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
    pub(crate) fn run_browser_commands(
        &mut self,
        commands: Vec<BrowserCommandRequest>,
        _cx: &mut Context<Self>,
    ) {
        for command in commands {
            self.post_browser_command_result(
                command.request_id,
                BrowserCommandOutcome::Error {
                    message: "embedded browser support is unavailable".to_owned(),
                },
            );
        }
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
    pub(crate) fn poll_browser_commands(&mut self, _cx: &mut Context<Self>) {}

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn flush_browser_state_updates(&mut self, cx: &mut Context<Self>) {
        let updates = self
            .browser
            .browser_views
            .iter_mut()
            .filter_map(|(pane_id, view)| {
                if view.in_flight_state.is_some() {
                    return None;
                }
                let state = view.pending_state.take()?;
                view.in_flight_state = Some(state.clone());
                Some((*pane_id, state.0, state.1))
            })
            .collect::<Vec<_>>();
        if updates.is_empty() {
            return;
        }

        let client = Arc::clone(&self.session.control_client);
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    updates
                        .into_iter()
                        .map(|(pane_id, url, title)| {
                            let result = session_call(
                                &client,
                                &ClientRequest::SetBrowserState {
                                    pane_id,
                                    url: url.clone(),
                                    title: title.clone(),
                                },
                            );
                            (pane_id, url, title, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let mut notify = false;
                let mut flush_again = false;
                for (pane_id, url, title, result) in results {
                    let submitted = (url.clone(), title.clone());
                    let Some(view) = this.browser.browser_views.get_mut(&pane_id) else {
                        continue;
                    };
                    if view.in_flight_state.as_ref() != Some(&submitted) {
                        continue;
                    }
                    view.in_flight_state = None;
                    match result {
                        Ok(ServiceResponse::Ack) => {
                            view.synced_url.clone_from(&url);
                            view.synced_title = title;
                            if view.pending_state.as_ref() == Some(&submitted) {
                                view.pending_state = None;
                            }
                            flush_again |= view.pending_state.is_some();
                            let current_url_matches = view.shared.borrow().url == url;
                            if current_url_matches
                                && let Some(editor) =
                                    this.editor.browser_url_editor.as_mut().filter(|editor| {
                                        editor.pane_id == pane_id && editor.replace_on_type
                                    })
                                && editor.text != url
                            {
                                editor.text = url;
                                notify = true;
                            }
                        }
                        // Keep the restored submission queued for
                        // `poll_once`, which calls this method every tick.
                        // Retrying here would create an unbounded failure loop.
                        Ok(response) => {
                            if view.pending_state.is_none() {
                                view.pending_state = Some(submitted);
                            }
                            this.session.connection_error =
                                Some(format!("unexpected response: {response:?}"));
                            notify = true;
                        }
                        // Keep the restored submission queued for the next
                        // bounded poll tick rather than retrying immediately.
                        Err(error) => {
                            if view.pending_state.is_none() {
                                view.pending_state = Some(submitted);
                            }
                            this.session.connection_error = Some(format!("{error:#}"));
                            notify = true;
                        }
                    }
                }
                if flush_again {
                    this.flush_browser_state_updates(cx);
                }
                if notify {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
    pub(crate) fn sync_browser_callback_state(&mut self, _cx: &mut Context<Self>) -> bool {
        false
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
    pub(crate) fn flush_browser_state_updates(&mut self, _cx: &mut Context<Self>) {}

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
    pub(crate) fn ensure_visible_browser_views(&mut self, _cx: &mut Context<Self>) {}

    pub(crate) fn render_browser_toolbar(
        &self,
        pane_id: Uuid,
        url: &str,
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self
            .editor
            .browser_url_editor
            .as_ref()
            .filter(|editor| editor.pane_id == pane_id);
        let address = editor.map_or_else(|| url.to_owned(), |editor| editor.text.clone());
        let editing = editor.is_some();
        let invalid = editor.is_some_and(|editor| editor.invalid);
        let selected = editor.is_some_and(|editor| editor.replace_on_type);
        let address_empty = address.is_empty();
        let address_text = if address_empty {
            "Enter a URL".to_owned()
        } else {
            address.clone()
        };
        let button_color = |enabled| rgb(if enabled { THEME.foreground } else { THEME.dim });
        div()
            .h(px(38.0))
            .flex_none()
            .px(px(8.0))
            .gap(px(5.0))
            .flex()
            .items_center()
            .bg(rgb(THEME.surface))
            .border_b_1()
            .border_color(rgb(THEME.border))
            .child(
                div()
                    .id(("browser-back", element_key(pane_id)))
                    .w(px(27.0))
                    .h(px(27.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(button_color(can_go_back))
                    .when(can_go_back, |element| {
                        element
                            .cursor_pointer()
                            .hover(|element| element.bg(rgb(THEME.elevated)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.browser_back(pane_id, cx);
                            }))
                    })
                    .child("‹"),
            )
            .child(
                div()
                    .id(("browser-forward", element_key(pane_id)))
                    .w(px(27.0))
                    .h(px(27.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(button_color(can_go_forward))
                    .when(can_go_forward, |element| {
                        element
                            .cursor_pointer()
                            .hover(|element| element.bg(rgb(THEME.elevated)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.browser_forward(pane_id, cx);
                            }))
                    })
                    .child("›"),
            )
            .child(
                div()
                    .id(("browser-reload", element_key(pane_id)))
                    .w(px(27.0))
                    .h(px(27.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| element.bg(rgb(THEME.elevated)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browser_reload(pane_id, loading, cx);
                    }))
                    .child(if loading { "×" } else { "↻" }),
            )
            .child(
                div()
                    .id(("browser-address", element_key(pane_id)))
                    .h(px(27.0))
                    .min_w(px(0.0))
                    .flex_1()
                    .px(px(9.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(rgb(if invalid {
                        THEME.danger
                    } else if editing {
                        THEME.accent
                    } else {
                        THEME.border_strong
                    }))
                    .bg(rgb(THEME.elevated))
                    .cursor_text()
                    .flex()
                    .items_center()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(if address_empty {
                        THEME.dim
                    } else {
                        THEME.foreground
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus_handle.focus(window);
                        let url = this
                            .browser_views_url(pane_id)
                            .unwrap_or_else(|| "about:blank".to_owned());
                        this.begin_browser_url_edit(pane_id, &url, cx);
                    }))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .overflow_hidden()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .when(selected && !address_empty, |element| {
                                        element
                                            .px(px(2.0))
                                            .rounded(px(2.0))
                                            .bg(rgb(THEME.accent))
                                            .text_color(rgb(0xffffff))
                                    })
                                    .child(address_text),
                            )
                            .when(editing, |element| {
                                element.child(
                                    div()
                                        .ml(px(1.0))
                                        .w(px(1.0))
                                        .h(px(15.0))
                                        .flex_none()
                                        .bg(rgb(THEME.accent)),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn browser_views_url(&self, pane_id: Uuid) -> Option<String> {
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        if let Some(view) = self.browser.browser_views.get(&pane_id) {
            return Some(view.shared.borrow().url.clone());
        }
        self.pane_metadata(pane_id)
            .and_then(|pane| match pane.kind {
                PaneKind::Browser { url } => Some(url),
                PaneKind::Terminal | PaneKind::Gallery => None,
            })
    }

    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    pub(crate) fn render_browser_workspace(
        &self,
        pane: &Pane,
        panes: &[Pane],
        _width: f32,
        _height: f32,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PaneKind::Browser { url } = &pane.kind else {
            return self.render_non_browser_pane(pane, panes, show_pane_header, cx);
        };
        let state = self.browser.browser_views.get(&pane.id).map(|view| {
            let state = view.shared.borrow();
            (
                state.url.clone(),
                state.loading,
                state.can_go_back,
                state.can_go_forward,
                Rc::clone(&view.pane),
                Rc::clone(&view.bounds),
            )
        });
        let (current_url, loading, can_go_back, can_go_forward) = state.as_ref().map_or_else(
            || (url.clone(), false, false, false),
            |(url, loading, back, forward, _, _)| (url.clone(), *loading, *back, *forward),
        );
        let toolbar = self.render_browser_toolbar(
            pane.id,
            &current_url,
            loading,
            can_go_back,
            can_go_forward,
            cx,
        );
        let content = if let Some((_, _, _, _, browser, last_bounds)) = state {
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    let rect = hh_cef_view::BrowserRect {
                        x: f32::from(bounds.origin.x),
                        y: f32::from(bounds.origin.y),
                        width: f32::from(bounds.size.width).max(1.0),
                        height: f32::from(bounds.size.height).max(1.0),
                    };
                    let parent_height = f32::from(window.bounds().size.height);
                    let next = (rect, parent_height);
                    if *last_bounds.borrow() != Some(next) {
                        *last_bounds.borrow_mut() = Some(next);
                        window.on_next_frame(move |_, _| browser.set_bounds(rect, parent_height));
                    }
                },
            )
            .size_full()
            .into_any_element()
        } else {
            self.render_browser_placeholder(
                self.browser
                    .browser_runtime_error
                    .as_deref()
                    .unwrap_or("Chromium is starting…"),
            )
        };
        let pane_id = pane.id;
        let drop_target = self
            .layout
            .dragging_pane
            .filter(|source| *source != pane_id)
            .map(|_| pane_id);
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .on_drag_move::<PaneDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let source = event.drag(cx).pane_id;
                    if source == pane_id {
                        return;
                    }
                    this.layout.dragging_pane = Some(source);
                    if let Some(placement) = split_placement_at(event.event.position, event.bounds)
                    {
                        this.layout.drag_hover.enter(DragDestination::Split {
                            target_pane: pane_id,
                            placement,
                        });
                    }
                    cx.notify();
                },
            ))
            .on_drag_move::<TabDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let Some(source) = event.drag(cx).pane_id else {
                        return;
                    };
                    if source == pane_id {
                        return;
                    }
                    this.layout.dragging_pane = Some(source);
                    if let Some(placement) = split_placement_at(event.event.position, event.bounds)
                    {
                        this.layout.drag_hover.enter(DragDestination::Split {
                            target_pane: pane_id,
                            placement,
                        });
                    }
                    cx.notify();
                },
            ))
            .when(show_pane_header, |element| {
                element.child(self.render_pane_header(panes, pane.id, cx))
            })
            .child(toolbar)
            .child(div().min_h(px(0.0)).flex_1().child(content))
            .when_some(drop_target, |element, target| {
                element.child(self.render_drop_layer(target, cx))
            })
            .into_any_element()
    }

    #[cfg(not(all(any(target_os = "macos", target_os = "linux"), feature = "browser")))]
    pub(crate) fn render_browser_workspace(
        &self,
        pane: &Pane,
        panes: &[Pane],
        _width: f32,
        _height: f32,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PaneKind::Browser { url } = &pane.kind else {
            return self.render_non_browser_pane(pane, panes, show_pane_header, cx);
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .when(show_pane_header, |element| {
                element.child(self.render_pane_header(panes, pane.id, cx))
            })
            .child(self.render_browser_toolbar(pane.id, url, false, false, false, cx))
            .child(
                div()
                    .min_h(px(0.0))
                    .flex_1()
                    .child(self.render_browser_placeholder(browser_unavailable_reason())),
            )
            .into_any_element()
    }

    /// A kind mismatch must degrade, never crash the render path.
    fn render_non_browser_pane(
        &self,
        pane: &Pane,
        panes: &[Pane],
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .when(show_pane_header, |element| {
                element.child(self.render_pane_header(panes, pane.id, cx))
            })
            .child(
                div()
                    .min_h(px(0.0))
                    .flex_1()
                    .child(self.render_browser_placeholder("This pane is not a browser tab")),
            )
            .into_any_element()
    }

    pub(crate) fn render_browser_placeholder(&self, message: &str) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .bg(rgb(THEME.terminal))
            .font_family(".SystemUIFont")
            .text_color(rgb(THEME.muted))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child("Chromium unavailable"),
            )
            .child(div().text_xs().child(message.to_owned()))
            .into_any_element()
    }
}
