#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::unreadable_literal,
    clippy::unused_self
)]

use futures::StreamExt;
use gpui::{
    App, AppContext, Application, Bounds, Context, FocusHandle, Image, ImageFormat, KeyBinding,
    Pixels, ScrollHandle, ShapedLine, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, point, px, size,
};
use hh_protocol::{
    AppearanceColor, ClientRequest, DEVELOPMENT_BUILD_ENV, HistoryArchiveStatus, HistoryClearScope,
    PaneStatus, PaneStreamState, ServiceResponse, SessionNotification, SessionSnapshot,
    StreamDiagnostics, TerminalScreen,
};
use hh_session_client::SessionClient;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
#[cfg(all(target_os = "macos", feature = "browser"))]
use std::ffi::c_void;
#[cfg(test)]
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use uuid::Uuid;

mod agent_icons;
mod appearance;
mod browser;
mod cli;
mod commands;
mod dialogs;
mod elements;
mod helpers;
mod history_settings;
mod image_transfer;
mod input;
mod menus;
mod notifications;
mod pane_identity;
mod panes;
mod pipeline;
mod reconcile;
mod render;
mod session;
mod sidebar;
mod terminal_view;
mod theme;
mod voice;
mod workspace_tab_strip;
mod workspaces;

mod typography;

mod ui_state;
mod updates;
mod view_models;

use agent_icons::{AgentIconAssets, CustomIcon, load_custom_icons};
use appearance::BannerArtwork;
#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
use browser::BrowserPaneView;
#[cfg(all(target_os = "linux", feature = "browser"))]
use browser::configure_linux_browser_backend;
#[cfg(all(target_os = "macos", feature = "browser"))]
use browser::native_nsview;
use browser::{BrowserUrlEditor, prepare_cef_process};
use commands::{AppConfig, ROOT_KEY_CONTEXT, ResolvedKeymap};
use helpers::{
    WorkspaceTabScope, default_sidebar_width, gpui_binding, migrated_sidebar_width,
    next_terminal_poll_delay_ms, product_name,
};
use session::session_call;
use theme::{AppTheme, BuiltInTheme};
use typography::TerminalFontProfile;
use ui_state::UiStateStore;
use updates::{UpdateCheckState, automatic_update_check_interval, automatic_update_checks_enabled};
use view_models::{
    ArchivedView, AssistantComposer, ColorPickerState, DragHoverState, HistoryEditor, Modal,
    PaneDrag, ResizeDrag, SelectionDrag, SidebarResizeLifecycle, SplitControlId, TabDropPreview,
    WorkspaceDropPreview,
};

actions!(
    hh_app,
    [
        NewWorkspace,
        ToggleSidebar,
        NewTab,
        NewBrowserTab,
        TerminalZoomIn,
        TerminalZoomOut,
        SplitRight,
        SplitDown,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        ShowCommandPalette,
        TogglePaneZoom,
        ShowNotifications,
        EqualizePanes,
        ReattachPane,
        RetryTerminalInput,
        ToggleVoiceMic,
        ShowSettings,
        ConsumeChordPrefix,
        CopyTerminal,
        PasteTerminal,
        FindTerminal,
        FindNextTerminal,
    ]
);

const DEFAULT_SIDEBAR_WIDTH: f32 = 144.0;
const MIN_SIDEBAR_WIDTH: f32 = 120.0;
const MAX_SIDEBAR_WIDTH: f32 = 420.0;
const DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH: f32 = 225.0;
const MIN_TERMINAL_AREA_WIDTH: f32 = 320.0;
const SIDEBAR_RESIZE_HIT_WIDTH: f32 = 8.0;
const SIDEBAR_RESIZE_VISUAL_WIDTH: f32 = 2.0;
const TITLEBAR_HEIGHT: f32 = 38.0;
const WORKSPACE_TAB_STRIP_HEIGHT: f32 = 32.0;
const APP_CHROME_HEIGHT: f32 = TITLEBAR_HEIGHT + WORKSPACE_TAB_STRIP_HEIGHT;
const MACOS_TRAFFIC_LIGHT_SAFE_INSET: f32 = 78.0;
const WORKSTATION_BANNER_ASPECT_RATIO: f32 = 3.0;
/// Pixel size of the bundled banner asset, asserted against the packaged file
/// by `bundled_workstation_banner_dimensions_match_the_packaged_asset`.
const BUNDLED_BANNER_PIXEL_WIDTH: u32 = 2172;
const BUNDLED_BANNER_PIXEL_HEIGHT: u32 = 724;
/// Rail-header bounds. The lower bound keeps a very wide image visible; the
/// upper bound stops a square or tall image from consuming the workstation list.
const WORKSTATION_BANNER_MIN_HEIGHT: f32 = 36.0;
const WORKSTATION_BANNER_MAX_HEIGHT: f32 = 260.0;
const PANE_HEADER_HEIGHT: f32 = 29.0;
const SPLIT_DIVIDER_SIZE: f32 = 4.0;
const TERMINAL_HORIZONTAL_PADDING: f32 = 18.0;
const TERMINAL_VERTICAL_PADDING: f32 = 12.0;
/// Keep the last PTY row clear of the viewport's clipped bottom edge.
const TERMINAL_BOTTOM_GUARD: f32 = 1.0;
const TERMINAL_FOCUS_BORDER_WIDTH: f32 = 1.0;
const MIN_PANE_WIDTH: f32 = 140.0;
const MIN_PANE_HEIGHT: f32 = 90.0;
const COMMAND_PALETTE_LIMIT: usize = 32;
const MAX_PASTE_BYTES: usize = 64 * 1024;
const ACTIVE_TERMINAL_POLL_MS: u64 = 33;
const DRAG_CLICK_SUPPRESSION_MS: u64 = 150;
const IDLE_TERMINAL_POLL_MS: u64 = 250;
const PTY_RESIZE_DEBOUNCE_MS: u64 = 16;
/// On-screen panes other than the focused one stream at this cadence so a
/// four-way split cannot multiply the focused pane's payload every 33 ms.
const SECONDARY_PANE_INTERVAL: Duration = Duration::from_millis(120);
const TAB_COLOR_ALPHA: u8 = 0xd0;
const STABLE_PRODUCT_NAME: &str = "Harness Harlot";
const DEVELOPMENT_PRODUCT_NAME: &str = "Harness Harlot Dev";
const THEME: AppTheme = BuiltInTheme::HarborNight.theme();

const fn pane_status_severity(status: PaneStatus) -> u8 {
    match status {
        PaneStatus::Idle => 0,
        PaneStatus::Done => 1,
        PaneStatus::Working => 2,
        PaneStatus::Attention => 3,
        PaneStatus::NeedsInput => 4,
        PaneStatus::NeedsApproval => 5,
    }
}

fn max_pane_status(statuses: impl IntoIterator<Item = PaneStatus>) -> PaneStatus {
    statuses
        .into_iter()
        .max_by_key(|status| pane_status_severity(*status))
        .unwrap_or_default()
}

const fn pane_status_color(status: PaneStatus) -> Option<u32> {
    match status {
        PaneStatus::Idle => None,
        PaneStatus::Working => Some(THEME.dim),
        PaneStatus::NeedsApproval => Some(THEME.danger),
        PaneStatus::NeedsInput => Some(THEME.accent),
        PaneStatus::Attention => Some(THEME.accent_soft),
        PaneStatus::Done => Some(THEME.ansi[2]),
    }
}
const APPEARANCE_PRESETS: [AppearanceColor; 8] = [
    AppearanceColor::new(0x62, 0xad, 0xff),
    AppearanceColor::new(0x67, 0xc8, 0xc6),
    AppearanceColor::new(0x95, 0xcc, 0x7f),
    AppearanceColor::new(0xe4, 0xbd, 0x72),
    AppearanceColor::new(0xef, 0x71, 0x7a),
    AppearanceColor::new(0xc9, 0x90, 0xe5),
    AppearanceColor::new(0xf0, 0x8a, 0xc0),
    AppearanceColor::DARK_GRAY,
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct AvailableUpdateBanner {
    version: String,
    requires_service_restart: bool,
    install_supported: bool,
    installing: bool,
}

type SharedSessionClient = Arc<Mutex<Option<SessionClient>>>;

impl AvailableUpdateBanner {
    fn label(&self) -> String {
        if self.installing {
            "Updating…".to_owned()
        } else if self.install_supported {
            format!("Update to {}", self.version)
        } else {
            format!("{} available — install manually", self.version)
        }
    }

    fn can_install(&self) -> bool {
        self.install_supported && !self.installing
    }
}
struct SessionState {
    /// Screen traffic only: pane updates, targeted pane snapshots, history
    /// status. Kept separate so a keystroke never waits behind a screen payload.
    stream_client: SharedSessionClient,
    /// Everything else except terminal input and selection updates.
    control_client: SharedSessionClient,
    /// Dedicated terminal-input connection, never blocked by generic control work.
    input_client: SharedSessionClient,
    /// Enqueue side of the stream lane's serialized request pipeline.
    stream_tx: futures::channel::mpsc::Sender<pipeline::PipelineJob>,
    /// Enqueue side of the control lane's serialized request pipeline.
    control_tx: futures::channel::mpsc::Sender<pipeline::PipelineJob>,
    /// Lossless/coalescing terminal-input lane, bounded by accepted bytes.
    terminal_input_tx: pipeline::TerminalInputSender,
    /// Interrupts an idle screen poll when input is queued. Capacity one
    /// coalesces a burst of keystrokes instead of scheduling a poll per byte.
    poll_wake_tx: futures::channel::mpsc::Sender<()>,
    snapshot: Option<SessionSnapshot>,
    screens: HashMap<Uuid, TerminalScreen>,
    pane_states: HashMap<Uuid, PaneStreamState>,
    notifications: Vec<SessionNotification>,
    notifications_latest_id: u64,
    /// When each pane's screen was last applied, used to pace on-screen panes
    /// other than the focused one.
    last_delivery: HashMap<Uuid, Instant>,
    window_active: bool,
    stream_diagnostics: StreamDiagnostics,
    connection_error: Option<String>,
    history_status: Option<HistoryArchiveStatus>,
}

struct SessionChannels {
    stream_client: SharedSessionClient,
    control_client: SharedSessionClient,
    input_client: SharedSessionClient,
    stream_tx: futures::channel::mpsc::Sender<pipeline::PipelineJob>,
    control_tx: futures::channel::mpsc::Sender<pipeline::PipelineJob>,
    terminal_input_tx: pipeline::TerminalInputSender,
    poll_wake_tx: futures::channel::mpsc::Sender<()>,
}

impl SessionState {
    fn new(
        channels: SessionChannels,
        window_active: bool,
        connection_error: Option<String>,
    ) -> Self {
        Self {
            stream_client: channels.stream_client,
            control_client: channels.control_client,
            input_client: channels.input_client,
            stream_tx: channels.stream_tx,
            control_tx: channels.control_tx,
            terminal_input_tx: channels.terminal_input_tx,
            poll_wake_tx: channels.poll_wake_tx,
            snapshot: None,
            screens: HashMap::new(),
            pane_states: HashMap::new(),
            notifications: Vec::new(),
            notifications_latest_id: 0,
            last_delivery: HashMap::new(),
            window_active,
            stream_diagnostics: StreamDiagnostics::default(),
            connection_error,
            history_status: None,
        }
    }
}

struct SidebarUi {
    active_workspace: Option<Uuid>,
    workspace_tab_scope: WorkspaceTabScope,
    expanded_workspaces: HashSet<Uuid>,
    collapsed_groups: HashSet<Uuid>,
    collapsed_pinned_sections: HashSet<Uuid>,
    collapsed_project_sections: HashSet<Uuid>,
    dismissed_workspace_tabs: HashSet<Uuid>,
    workstation_tab_scroll: ScrollHandle,
    dragging_workspace: Option<Uuid>,
    workspace_drop_preview: Option<WorkspaceDropPreview>,
    suppress_workspace_click_until: Option<Instant>,
    tab_drop_preview: Option<TabDropPreview>,
    suppress_tab_click_until: Option<Instant>,
    sidebar_resize: SidebarResizeLifecycle,
    preferred_sidebar_width: f32,
    sidebar_visible: bool,
    sidebar_activity: bool,
    sidebar_pixels: f32,
    workstation_banner: Option<BannerArtwork>,
    workstation_banner_hidden: bool,
}

impl SidebarUi {
    fn new(
        preferred_sidebar_width: f32,
        workstation_banner: Option<BannerArtwork>,
        workstation_banner_hidden: bool,
    ) -> Self {
        Self {
            active_workspace: None,
            workspace_tab_scope: WorkspaceTabScope::Workstation,
            expanded_workspaces: HashSet::new(),
            collapsed_groups: HashSet::new(),
            collapsed_pinned_sections: HashSet::new(),
            collapsed_project_sections: HashSet::new(),
            dismissed_workspace_tabs: HashSet::new(),
            workstation_tab_scroll: ScrollHandle::new(),
            dragging_workspace: None,
            workspace_drop_preview: None,
            suppress_workspace_click_until: None,
            tab_drop_preview: None,
            suppress_tab_click_until: None,
            sidebar_resize: SidebarResizeLifecycle::default(),
            preferred_sidebar_width,
            sidebar_visible: true,
            sidebar_activity: false,
            sidebar_pixels: default_sidebar_width(),
            workstation_banner,
            workstation_banner_hidden,
        }
    }
}

struct LayoutUi {
    focused_pane: Option<Uuid>,
    split_ratios: HashMap<SplitControlId, f32>,
    zoomed_pane: Option<Uuid>,
    resizing: Option<ResizeDrag>,
    dragging_pane: Option<Uuid>,
    drag_hover: DragHoverState,
    selection_drag: Option<SelectionDrag>,
    last_sizes: HashMap<Uuid, (u16, u16)>,
    resize_generation: u64,
    workspace_pixels: (f32, f32),
}

impl LayoutUi {
    fn new() -> Self {
        Self {
            focused_pane: None,
            split_ratios: HashMap::new(),
            zoomed_pane: None,
            resizing: None,
            dragging_pane: None,
            drag_hover: DragHoverState::default(),
            selection_drag: None,
            last_sizes: HashMap::new(),
            resize_generation: 0,
            workspace_pixels: (0.0, 0.0),
        }
    }
}

struct EditorUi {
    modal: Modal,
    history_editor: Option<HistoryEditor>,
    history_clear_confirmation: Option<HistoryClearScope>,
    color_picker: Option<ColorPickerState>,
    browser_url_editor: Option<BrowserUrlEditor>,
    assistant_composer: Option<AssistantComposer>,
    ime_preedit: String,
    workspace_input_focus: [FocusHandle; 4],
    workspace_input_layouts: [Option<ShapedLine>; 4],
    workspace_input_bounds: [Option<Bounds<Pixels>>; 4],
    /// Archived-history views per pane; belongs with editing UI state.
    archived_views: HashMap<Uuid, ArchivedView>,
    update_available: Option<AvailableUpdateBanner>,
    update_check: UpdateCheckState,
}

impl EditorUi {
    fn new(workspace_input_focus: [FocusHandle; 4]) -> Self {
        Self {
            modal: Modal::None,
            history_editor: None,
            history_clear_confirmation: None,
            color_picker: None,
            browser_url_editor: None,
            assistant_composer: None,
            ime_preedit: String::new(),
            workspace_input_focus,
            workspace_input_layouts: [None, None, None, None],
            workspace_input_bounds: [None, None, None, None],
            archived_views: HashMap::new(),
            update_available: None,
            update_check: UpdateCheckState::default(),
        }
    }
}

#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
struct BrowserUi {
    browser_views: HashMap<Uuid, BrowserPaneView>,
    #[cfg(target_os = "macos")]
    browser_parent_view: Option<*mut c_void>,
    browser_runtime_initialized: bool,
    browser_runtime_error: Option<String>,
    cef_shutdown_subscription: Option<gpui::Subscription>,
}

#[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
impl BrowserUi {
    #[cfg(target_os = "macos")]
    fn new(browser_parent_view: Option<*mut c_void>) -> Self {
        Self {
            browser_views: HashMap::new(),
            browser_parent_view,
            browser_runtime_initialized: false,
            browser_runtime_error: None,
            cef_shutdown_subscription: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn new() -> Self {
        Self {
            browser_views: HashMap::new(),
            browser_runtime_initialized: false,
            browser_runtime_error: None,
            cef_shutdown_subscription: None,
        }
    }
}

struct HhApp {
    focus_handle: FocusHandle,
    keymap: ResolvedKeymap,
    terminal_font: TerminalFontProfile,
    terminal_zoom_levels: HashMap<Uuid, i8>,
    terminal_shape_cache: RefCell<HashMap<Uuid, elements::PaneShapeCache>>,
    custom_icons: Vec<CustomIcon>,
    ui_state_store: Option<UiStateStore>,
    session: SessionState,
    sidebar: SidebarUi,
    layout: LayoutUi,
    editor: EditorUi,
    voice: voice::VoiceUi,
    #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
    browser: BrowserUi,
}

impl HhApp {
    fn new(window: &mut Window, keymap: ResolvedKeymap, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        if let Err(error) = image_transfer::cleanup_clipboard_image_cache() {
            eprintln!("Harness Harlot clipboard image cleanup unavailable: {error:#}");
        }
        let workspace_input_focus = [
            cx.focus_handle(),
            cx.focus_handle(),
            cx.focus_handle(),
            cx.focus_handle(),
        ];
        let terminal_font = TerminalFontProfile::resolve(cx.text_system());
        let ui_state_store = match UiStateStore::from_default_path() {
            Ok(store) => Some(store),
            Err(error) => {
                eprintln!("Harness Harlot UI state unavailable: {error:#}");
                None
            }
        };
        let stored_sidebar_width = Self::load_ui_state(
            ui_state_store.as_ref(),
            "UI state ignored",
            |store| store.load_workspace_sidebar_width(),
            None,
        );
        let preferred_sidebar_width = migrated_sidebar_width(stored_sidebar_width);
        if development_build() && stored_sidebar_width != Some(preferred_sidebar_width) {
            Self::load_ui_state(
                ui_state_store.as_ref(),
                "sidebar default correction was not persisted",
                |store| store.save_workspace_sidebar_width(preferred_sidebar_width),
                (),
            );
        }
        let workstation_banner = ui_state_store.as_ref().and_then(|store| {
            Self::load_ui_state(
                Some(store),
                "custom workstation banner ignored",
                |store| store.load_workstation_banner(),
                None,
            )
            .map(|stored| BannerArtwork {
                image: Arc::new(Image::from_bytes(ImageFormat::Png, stored.png)),
                width: stored.width,
                height: stored.height,
            })
        });
        let workstation_banner_hidden = ui_state_store.as_ref().is_some_and(|store| {
            Self::load_ui_state(
                Some(store),
                "banner visibility ignored",
                |store| store.load_workstation_banner_hidden(),
                false,
            )
        });
        // Preserve startup failures for the banner. `None` is transient:
        // each pipeline lane reconnects lazily in `with_session_client` before
        // its first request, including the control lane used for terminal input.
        let (stream_client, startup_connection_error) = match SessionClient::connect() {
            Ok(client) => (Arc::new(Mutex::new(Some(client))), None),
            Err(error) => (Arc::new(Mutex::new(None)), Some(format!("{error:#}"))),
        };
        let (control_client, second_error) = match SessionClient::connect() {
            Ok(client) => (Arc::new(Mutex::new(Some(client))), None),
            Err(error) => (Arc::new(Mutex::new(None)), Some(format!("{error:#}"))),
        };
        let (input_client, third_error) = match SessionClient::connect() {
            Ok(client) => (Arc::new(Mutex::new(Some(client))), None),
            Err(error) => (Arc::new(Mutex::new(None)), Some(format!("{error:#}"))),
        };
        let startup_connection_error = startup_connection_error.or(second_error).or(third_error);
        let (stream_tx, stream_lane) = pipeline::bounded_lane(pipeline::STREAM_PIPELINE_CAPACITY);
        let (control_tx, control_lane) =
            pipeline::bounded_lane(pipeline::CONTROL_PIPELINE_CAPACITY);
        let (terminal_input_tx, terminal_input_lane) =
            pipeline::terminal_input_channel(pipeline::TERMINAL_INPUT_CAPACITY_BYTES);
        let (poll_wake_tx, mut poll_wake_rx) = futures::channel::mpsc::channel(1);
        #[cfg(all(target_os = "macos", feature = "browser"))]
        let browser_parent_view = native_nsview(window);
        let mut app = Self {
            focus_handle,
            keymap,
            terminal_font,
            terminal_zoom_levels: HashMap::new(),
            terminal_shape_cache: RefCell::new(HashMap::new()),
            custom_icons: load_custom_icons(),
            ui_state_store,
            session: SessionState::new(
                SessionChannels {
                    stream_client,
                    control_client,
                    input_client,
                    stream_tx,
                    control_tx,
                    terminal_input_tx,
                    poll_wake_tx,
                },
                window.is_window_active(),
                startup_connection_error,
            ),
            sidebar: SidebarUi::new(
                preferred_sidebar_width,
                workstation_banner,
                workstation_banner_hidden,
            ),
            layout: LayoutUi::new(),
            editor: EditorUi::new(workspace_input_focus),
            voice: voice::VoiceUi::new(),
            #[cfg(all(target_os = "macos", feature = "browser"))]
            browser: BrowserUi::new(browser_parent_view),
            #[cfg(all(target_os = "linux", feature = "browser"))]
            browser: BrowserUi::new(),
        };
        app.update_window_geometry(window);
        app.initial_state_fetch(cx);
        app.refresh_notifications();
        if app.layout.focused_pane.is_some() && app.session.screens.is_empty() {
            app.initial_state_fetch(cx);
        }
        pipeline::spawn_lane(cx, stream_lane, |app: &HhApp| &app.session.stream_client);
        pipeline::spawn_lane(cx, control_lane, |app: &HhApp| &app.session.control_client);
        pipeline::spawn_terminal_input_lane(cx, terminal_input_lane, |app: &HhApp| {
            &app.session.input_client
        });
        #[cfg(all(any(target_os = "macos", target_os = "linux"), feature = "browser"))]
        {
            app.browser.cef_shutdown_subscription = Some(cx.on_app_quit(|this, _| {
                this.browser.browser_views.clear();
                async {
                    hh_cef_view::shutdown_runtime();
                }
            }));
        }
        app.voice.quit_subscription = Some(cx.on_app_quit(|this, _| {
            this.shutdown_voice();
            async {}
        }));

        cx.observe_window_bounds(window, |this, window, cx| {
            if this.update_window_geometry(window) {
                this.sync_pty_sizes(cx);
                cx.notify();
            }
        })
        .detach();

        cx.observe_window_activation(window, |this, window, cx| {
            this.session.window_active = window.is_window_active();
            if this.session.window_active {
                if let Some(pane_id) = this.layout.focused_pane
                    && this.auto_read_pane_notifications(pane_id, cx)
                {
                    cx.notify();
                }
            } else {
                this.cancel_sidebar_resize(window, cx);
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let mut poll_delay_ms = ACTIVE_TERMINAL_POLL_MS;
            loop {
                let delay = futures::FutureExt::fuse(gpui::Timer::after(Duration::from_millis(
                    poll_delay_ms,
                )));
                let input_wake = futures::FutureExt::fuse(poll_wake_rx.next());
                futures::pin_mut!(delay, input_wake);
                futures::select_biased! {
                    _ = input_wake => poll_delay_ms = ACTIVE_TERMINAL_POLL_MS,
                    _ = delay => {}
                }
                let Some(state_changed) = pipeline::poll_once(&this, cx).await else {
                    break;
                };
                poll_delay_ms = next_terminal_poll_delay_ms(poll_delay_ms, state_changed);
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_secs(5)).await;
                let Ok(client) = this.update(cx, |this, _| Arc::clone(&this.session.stream_client))
                else {
                    break;
                };
                let response = cx
                    .background_spawn(async move {
                        session_call(&client, &ClientRequest::GetHistoryStatus)
                    })
                    .await;
                let Ok(()) = this.update(cx, |this, cx| {
                    if this.apply_history_status_result(response) {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
        if automatic_update_checks_enabled() {
            cx.spawn(async move |this, cx| {
                let mut first_check = true;
                loop {
                    gpui::Timer::after(if first_check {
                        Duration::from_secs(10)
                    } else {
                        automatic_update_check_interval()
                    })
                    .await;
                    first_check = false;
                    let Ok(()) = this.update(cx, |this, cx| this.check_for_updates(cx)) else {
                        break;
                    };
                }
            })
            .detach();
        }
        app
    }
    /// Screen traffic: pane updates, targeted pane snapshots, history
    /// status. Kept only for the synchronous startup fetch; everything
    /// else flows through the async pipelines.
    fn stream_call(&self, request: &ClientRequest) -> anyhow::Result<ServiceResponse> {
        session_call(&self.session.stream_client, request)
    }

    fn report(&mut self, error: &anyhow::Error) {
        self.session.connection_error = Some(format!("{error:#}"));
    }

    fn report_unexpected(&mut self, response: &ServiceResponse) {
        self.session.connection_error = Some(format!("unexpected response: {response:?}"));
    }

    /// Runs one UI-state operation, reporting a failure without interrupting
    /// startup or interaction. `fallback` keeps the caller going.
    pub(crate) fn load_ui_state<T>(
        store: Option<&UiStateStore>,
        label: &str,
        operation: impl FnOnce(&UiStateStore) -> anyhow::Result<T>,
        fallback: T,
    ) -> T {
        match store.map(operation) {
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                eprintln!("Harness Harlot {label}: {error:#}");
                fallback
            }
            None => fallback,
        }
    }
}

/// Starts the bundled session service only when no compatible local service is
/// reachable. The service is deliberately detached from the desktop lifetime:
/// closing or replacing the app UI never asks it to stop, preserving active
/// terminal sessions. A future updater must instead defer until the service is
/// explicitly quiescent (see `docs/macos-release.md`).
fn ensure_bundled_session_service() {
    if std::env::var_os("HH_DISABLE_BUNDLED_SERVICE").is_some()
        || SessionClient::connect()
            .and_then(|mut client| client.call(&ClientRequest::GetSnapshot))
            .is_ok()
    {
        return;
    }

    if SessionClient::legacy_service_is_listening() {
        eprintln!(
            "Harness Harlot found an older session service with live PTYs at the legacy socket; refusing to start a second service. Close those sessions and stop the old service before relaunching this version."
        );
        return;
    }

    let Some(service) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join("hh-service")))
    else {
        return;
    };
    if !service.is_file() {
        return;
    }
    if let Err(error) = Command::new(service).spawn() {
        eprintln!("Harness Harlot could not start its bundled session service: {error}");
        return;
    }

    for _ in 0..20 {
        thread::sleep(Duration::from_millis(50));
        if SessionClient::connect()
            .and_then(|mut client| client.call(&ClientRequest::GetSnapshot))
            .is_ok()
        {
            return;
        }
    }
    eprintln!("Harness Harlot session service did not become ready within one second");
}

fn development_build() -> bool {
    cfg!(debug_assertions) || std::env::var(DEVELOPMENT_BUILD_ENV).as_deref() == Ok("1")
}

/// Prefer the copy packaged inside a native macOS bundle. The source-tree path
/// keeps local non-bundled development builds visually faithful as well.
#[cfg(test)]
fn workstation_banner_path() -> PathBuf {
    let bundled = std::env::current_exe().ok().and_then(|executable| {
        executable
            .parent()
            .and_then(|macos_directory| macos_directory.parent())
            .map(|contents_directory| {
                contents_directory
                    .join("Resources")
                    .join("harnessharlot-banner.png")
            })
    });
    bundled.filter(|path| path.is_file()).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("harnessharlot-banner.png")
    })
}

/// Sets the live Dock icon explicitly. `AppKit` otherwise retains the generic
/// placeholder selected while a development bundle is being rebuilt in place.
#[cfg(target_os = "macos")]
fn install_macos_dock_icon(development_build: bool) {
    hh_macos_icon::install_dock_icon(development_build);
}

#[cfg(not(target_os = "macos"))]
fn install_macos_dock_icon(_: bool) {}

#[cfg(target_os = "macos")]
fn exclude_history_from_backup() {
    let Some(history) = hh_protocol::state_directory().map(|directory| directory.join("history"))
    else {
        return;
    };
    if history.is_dir()
        && let Err(error) = hh_macos_icon::exclude_directory_from_backup(&history)
    {
        eprintln!("Harness Harlot could not exclude local history from backups: {error}");
    }
}

#[cfg(not(target_os = "macos"))]
fn exclude_history_from_backup() {}

/// Installs a panic hook that appends a timestamped, symbolized entry to
/// `<state_dir>/panic.log`, or an owner-only temporary fallback, truncates it
/// past 1 MiB, and then delegates to the previous hook so stderr output is
/// preserved. Purely local diagnostics: nothing leaves the machine.
fn panic_log_directory() -> Option<std::path::PathBuf> {
    hh_protocol::state_directory()
        .filter(|directory| hh_protocol::ensure_private_directory(directory).is_ok())
        .or_else(|| {
            let directory = std::env::temp_dir().join(format!(
                "harness-harlot-{}-panic",
                rustix::process::geteuid().as_raw()
            ));
            hh_protocol::ensure_private_directory(&directory)
                .is_ok()
                .then_some(directory)
        })
}

fn install_panic_log() {
    let state_dir = panic_log_directory();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let log = state_dir
            .as_ref()
            .map(|directory| directory.join("panic.log"));
        let append_entry = |path: &std::path::Path| {
            use std::io::Write as _;
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

            static PANIC_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _guard = PANIC_LOG_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            if file.metadata()?.len() > 1024 * 1024 {
                file.set_len(0)?;
            }
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("<unnamed>");
            let payload = info.payload_as_str().unwrap_or("non-string panic payload");
            let backtrace = std::backtrace::Backtrace::force_capture();
            let timestamp = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unavailable".to_owned());
            writeln!(
                file,
                "{timestamp} version {} thread {thread_name}\npanic: {payload}\n{backtrace}\n",
                env!("CARGO_PKG_VERSION")
            )
        };
        if let Some(log) = log {
            let _ = append_entry(&log);
        }
        previous_hook(info);
    }));
}

fn main() {
    install_panic_log();
    prepare_cef_process();
    match cli::run_cli_or_request_desktop() {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            eprintln!("hh: {error:#}");
            std::process::exit(1);
        }
    }
    if let Err(error) = cli::ensure_packaged_cli_link() {
        eprintln!("hh: could not install ~/.local/bin/hh: {error:#}");
    }
    #[cfg(all(target_os = "linux", feature = "browser"))]
    configure_linux_browser_backend();
    let development_build = development_build();
    let product_name = product_name(development_build);
    ensure_bundled_session_service();
    exclude_history_from_backup();
    Application::new()
        .with_assets(AgentIconAssets)
        .run(move |cx: &mut App| {
            install_macos_dock_icon(development_build);
            let keymap = match AppConfig::load().and_then(|config| config.resolve_keymap()) {
                Ok(keymap) => keymap,
                Err(error) => {
                    eprintln!("Harness Harlot config ignored: {error}");
                    AppConfig::default()
                        .resolve_keymap()
                        .expect("built-in keymap must be valid")
                }
            };
            let mut bindings = keymap.bindings.iter().map(gpui_binding).collect::<Vec<_>>();
            bindings.extend(
                keymap.chord_prefixes.iter().map(|prefix| {
                    KeyBinding::new(prefix, ConsumeChordPrefix, Some(ROOT_KEY_CONTEXT))
                }),
            );
            bindings.extend([
                KeyBinding::new("cmd-c", CopyTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("cmd-v", PasteTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("cmd-f", FindTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("cmd-g", FindNextTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("ctrl-shift-c", CopyTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("ctrl-shift-v", PasteTerminal, Some(ROOT_KEY_CONTEXT)),
                KeyBinding::new("ctrl-shift-f", FindTerminal, Some(ROOT_KEY_CONTEXT)),
            ]);
            cx.bind_keys(bindings);
            let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitlebarOptions {
                        title: Some(product_name.into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(13.0), px(13.0))),
                    }),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(720.0), px(460.0))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| HhApp::new(window, keymap.clone(), cx)),
            )
            .expect("open Harness Harlot window");
            cx.activate(true);
        });
}

#[cfg(test)]
mod tests {
    use super::{
        AvailableUpdateBanner, BUNDLED_BANNER_PIXEL_HEIGHT, BUNDLED_BANNER_PIXEL_WIDTH,
        max_pane_status, pane_status_color, workstation_banner_path,
    };
    use hh_protocol::PaneStatus;

    #[test]
    fn update_banner_reflects_install_capability() {
        let installable = AvailableUpdateBanner {
            version: "0.2.0".to_owned(),
            requires_service_restart: true,
            install_supported: true,
            installing: false,
        };
        assert_eq!(installable.label(), "Update to 0.2.0");
        assert!(installable.can_install());

        let manual = AvailableUpdateBanner {
            install_supported: false,
            ..installable
        };
        assert_eq!(manual.label(), "0.2.0 available — install manually");
        assert!(!manual.can_install());
    }

    #[test]
    fn pane_status_badges_use_declared_severity_and_colors() {
        assert_eq!(
            max_pane_status([
                PaneStatus::Done,
                PaneStatus::Working,
                PaneStatus::Attention,
                PaneStatus::NeedsInput,
                PaneStatus::NeedsApproval,
            ]),
            PaneStatus::NeedsApproval
        );
        assert_eq!(max_pane_status([]), PaneStatus::Idle);
        assert_eq!(pane_status_color(PaneStatus::Idle), None);
        for status in [
            PaneStatus::Done,
            PaneStatus::Working,
            PaneStatus::Attention,
            PaneStatus::NeedsInput,
            PaneStatus::NeedsApproval,
        ] {
            assert!(pane_status_color(status).is_some(), "status: {status:?}");
        }
    }

    #[test]
    fn workstation_banner_resolves_to_a_local_packaged_resource() {
        let banner = workstation_banner_path();
        assert!(banner.is_file(), "banner is missing: {}", banner.display());
        assert_eq!(
            banner.file_name().and_then(|name| name.to_str()),
            Some("harnessharlot-banner.png")
        );
    }

    #[test]
    fn bundled_workstation_banner_dimensions_match_the_packaged_asset() {
        let bytes = include_bytes!("../assets/harnessharlot-banner.png");
        let dimensions = image::ImageReader::new(std::io::Cursor::new(bytes.as_slice()))
            .with_guessed_format()
            .expect("guess bundled banner format")
            .into_dimensions()
            .expect("read bundled banner dimensions");
        assert_eq!(
            dimensions,
            (BUNDLED_BANNER_PIXEL_WIDTH, BUNDLED_BANNER_PIXEL_HEIGHT)
        );
    }
}
