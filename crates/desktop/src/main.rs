#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::unused_self
)]

use anyhow::Context as _;
#[cfg(all(target_os = "macos", feature = "browser"))]
use gpui::canvas;
use gpui::{
    AnyElement, App, Application, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle,
    DispatchPhase, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, GlobalElementId, Hitbox, HitboxBehavior, Image, ImageFormat, InspectorElementId,
    InteractiveElement, IntoElement, KeyBinding, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, PathPromptOptions, Pixels, Point,
    ScrollHandle, ScrollWheelEvent, ShapedLine, StrikethroughStyle, Style, StyledText, TextRun,
    TitlebarOptions, UTF16Selection, UnderlineStyle, Window, WindowBounds, WindowOptions, actions,
    div, fill, img, point, prelude::*, px, relative, rgb, rgba, size, svg,
};
use hh_desktop::SessionClient;
use hh_protocol::{
    AppearanceColor, ClientRequest, DEVELOPMENT_BUILD_ENV, DropPlacement, HistoryArchiveStatus,
    HistoryCleanupPolicy, HistoryClearScope, HistoryPageDirection, HistoryPageFlags,
    HistoryRetention, HistorySettings, HistoryWarning, MAX_SSH_INPUT_LEN, NotificationKind, Pane,
    PaneKind, PaneLayout, PaneRevisionCursor, PaneStreamState, ServiceResponse,
    SessionNotification, SessionSnapshot, SplitAxis, StreamDiagnostics, TerminalAttributes,
    TerminalColor, TerminalHistoryPage, TerminalLine, TerminalModes, TerminalModifiers,
    TerminalMouseAction, TerminalMouseButton, TerminalPoint, TerminalProfile, TerminalRun,
    TerminalScreen, TerminalSelection, TerminalSelectionKind, TmuxScanScope, TmuxSession,
    TmuxSessionId, Workspace, WorkspaceConnection, WorkspaceConnectionStatus, normalize_ssh_input,
    validate_ssh_host, validate_workspace_dir,
};
use parking_lot::Mutex;
#[cfg(all(target_os = "macos", feature = "browser"))]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(all(target_os = "macos", feature = "browser"))]
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
#[cfg(all(target_os = "macos", feature = "browser"))]
use std::ffi::c_void;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(all(target_os = "macos", feature = "browser"))]
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

mod agent_icons;
mod commands;
mod helpers;
mod theme;
mod typography;
mod ui_state;
mod view_models;

use agent_icons::{
    AgentIconAssets, AgentIconFormat, CustomIcon, agent_icon_definition, import_custom_icon,
    load_custom_icons,
};
use commands::{
    AppCommand, AppConfig, ROOT_KEY_CONTEXT, ResolvedBinding, ResolvedKeymap, descriptor,
    palette_matches,
};
use helpers::{
    FocusResync, HeaderDropZone, IDENTITY_MARK_SIZE, SidebarSection, WorkspaceTabScope,
    WorkstationTabEntry, abbreviate_home, append_rename_text, apply_layout_control_mutation,
    banner_fit_size, click_suppression_active, collect_pane_sizes, collect_terminal_tabs,
    composite_rgb, constrained_sidebar_width, default_sidebar_width, effective_split_ratio,
    element_key, find_pane, find_split_rect, focus_resync_for, format_bytes, format_history_date,
    gpui_binding, header_drop_zone, history_label, history_scope_key, history_warning_text,
    inactive_stack_contains, migrated_sidebar_width, next_terminal_poll_delay_ms,
    paced_subscriptions, pane_update_requires_repaint, parse_hex_color,
    partition_workstation_entries, plain_history_line, prepare_paste, product_name,
    readable_text_color, render_bell_icon, render_sidebar_toggle_icon,
    render_terminal_profile_icon, render_terminal_profile_mark, resolved_terminal_accent,
    resolved_workspace_color, rgba_with_alpha, selection_span, sidebar_width_for_visibility,
    split_child_dimensions, split_control_id, split_element_key, split_placement_at,
    split_target_for_drag, split_target_for_drag_ids, tab_identity_presentation,
    terminal_input_bytes, terminal_modifiers, terminal_mouse_button, terminal_point_at,
    terminal_run_display_text, terminal_tab_count_label, terminal_tab_secondary_label,
    visible_panes, workspace_is_selectable, workspace_layout_for_focused_pane,
    workspace_pixel_size, workspace_scope_for_tab, workspace_strip_active_tab,
    workspace_tab_click_target, workspace_tab_entries, workspace_tab_focus_target,
    workspace_tab_set, workspace_tab_standalone_pane, workspace_terminal_tabs,
    workspace_visible_panes, workstation_banner_header_height, zoom_projection,
};
use theme::{AppTheme, BuiltInTheme};
use typography::{TerminalCellMetrics, TerminalFontProfile, adjusted_terminal_zoom_level};
use ui_state::UiStateStore;
use view_models::{
    ArchivedView, CloseConfirmation, ColorPickerState, ColorTarget, CommandPaletteState,
    CreateMenu, CreateMenuTarget, DialogAction, DialogSpec, DialogTextEditor, DialogTone,
    DirEditor, DirEditorTarget, DragDestination, DragHoverState, GroupMenu, GroupRenameEditor,
    HistoryEditField, HistoryEditor, LayoutControlMutation, Modal, PaneControlIcon, PaneDrag,
    PixelRect, RenameEditor, RenameTarget, ResizeDrag, SearchEditor, SelectionDrag,
    SidebarResizeLifecycle, SidebarResizeMove, SplitControlId, TabCloseConfirmation, TabDrag,
    TabDropPreview, TabIdentityPresentation, TabMenu, TerminalLineRender, TmuxSelectionChange,
    TmuxSessionPicker, TooltipView, WorkspaceConnectionInfo, WorkspaceCreationDialog,
    WorkspaceCreationField, WorkspaceCreationKind, WorkspaceCreationStep,
    WorkspaceDeleteConfirmation, WorkspaceDisconnectConfirmation, WorkspaceDrag,
    WorkspaceDropPreview, WorkspaceMenu, WorkspaceRenameEditor, route_workspace_creation_paste,
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
const APP_CHROME_HEIGHT: f32 = TITLEBAR_HEIGHT;
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
/// Budget the settings preview fits inside, borders excluded.
const SETTINGS_BANNER_PREVIEW_MAX_WIDTH: f32 = 420.0;
const SETTINGS_BANNER_PREVIEW_MAX_HEIGHT: f32 = 220.0;
const PANE_HEADER_HEIGHT: f32 = 29.0;
const SPLIT_DIVIDER_SIZE: f32 = 4.0;
const TERMINAL_HORIZONTAL_PADDING: f32 = 18.0;
const TERMINAL_VERTICAL_PADDING: f32 = 12.0;
const TERMINAL_FOCUS_BORDER_WIDTH: f32 = 1.0;
const MIN_PANE_WIDTH: f32 = 140.0;
const MIN_PANE_HEIGHT: f32 = 90.0;
const COMMAND_PALETTE_LIMIT: usize = 32;
const MAX_PASTE_BYTES: usize = 64 * 1024;
const ACTIVE_TERMINAL_POLL_MS: u64 = 33;
const DRAG_CLICK_SUPPRESSION_MS: u64 = 150;
const IDLE_TERMINAL_POLL_MS: u64 = 250;
/// Polling cadence once nothing has produced output and nobody has typed for
/// `DEEP_IDLE_AFTER`. Pane states still arrive at this rate, so the first byte
/// of new output restores the active cadence within one poll.
const DEEP_IDLE_POLL_MS: u64 = 2_000;
const DEEP_IDLE_AFTER: Duration = Duration::from_hours(1);
const PTY_RESIZE_DEBOUNCE_MS: u64 = 16;
/// On-screen panes other than the focused one stream at this cadence so a
/// four-way split cannot multiply the focused pane's payload every 33 ms.
const SECONDARY_PANE_INTERVAL: Duration = Duration::from_millis(120);
const TAB_COLOR_ALPHA: u8 = 0xd0;
const STABLE_PRODUCT_NAME: &str = "Harness Harlot";
const DEVELOPMENT_PRODUCT_NAME: &str = "Harness Harlot Dev";
const THEME: AppTheme = BuiltInTheme::HarborNight.theme();
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

type SharedSessionClient = Arc<Mutex<Option<SessionClient>>>;
fn session_call(
    client: &SharedSessionClient,
    request: &ClientRequest,
) -> anyhow::Result<ServiceResponse> {
    let mut client = client.lock();
    if client.is_none() {
        *client = Some(SessionClient::connect()?);
    }
    let result = client
        .as_mut()
        .expect("session client initialized")
        .call(request);
    if result.is_err() {
        *client = None;
    }
    result
}

fn session_notify(client: &SharedSessionClient, request: &ClientRequest) -> anyhow::Result<()> {
    let mut client = client.lock();
    if client.is_none() {
        *client = Some(SessionClient::connect()?);
    }
    let result = client
        .as_mut()
        .expect("session client initialized")
        .notify(request);
    if result.is_err() {
        *client = None;
    }
    result
}

/// A banner ready to render: decoded-image handle plus its pixel dimensions,
/// which drive rail-header height and preview sizing.
#[derive(Clone, Debug)]
struct BannerArtwork {
    image: Arc<Image>,
    width: u32,
    height: u32,
}

impl BannerArtwork {
    // Banner dimensions are capped at 8,192 px, so both integer values are
    // exactly representable as f32.
    #[allow(clippy::cast_precision_loss)]
    fn aspect_ratio(&self) -> f32 {
        self.width as f32 / self.height.max(1) as f32
    }
}

#[derive(Clone, Debug)]
struct BrowserUrlEditor {
    pane_id: Uuid,
    text: String,
    replace_on_type: bool,
    invalid: bool,
}

fn browser_key_text(key: &str, key_char: Option<&str>, shift: bool, alt: bool) -> Option<String> {
    let mut text = key_char.map(ToOwned::to_owned).or_else(|| {
        if alt {
            return None;
        }
        match key {
            "space" => Some(" ".to_owned()),
            key if key.chars().count() == 1 && !key.chars().any(char::is_control) => {
                Some(key.to_owned())
            }
            _ => None,
        }
    })?;
    if shift {
        text.make_ascii_uppercase();
    }
    Some(text)
}

#[cfg(all(target_os = "macos", feature = "browser"))]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct BrowserShared {
    url: String,
    title: Option<String>,
    favicon: Option<Arc<Image>>,
    loading: bool,
    can_go_back: bool,
    can_go_forward: bool,
    dirty: bool,
}

#[cfg(all(target_os = "macos", feature = "browser"))]
#[derive(Debug)]
struct BrowserPaneView {
    pane: Rc<hh_cef_view::BrowserPane>,
    shared: Rc<RefCell<BrowserShared>>,
    last_snapshot_url: String,
    synced_url: String,
    synced_title: Option<String>,
    pending_state: Option<(String, Option<String>)>,
    in_flight_state: Option<(String, Option<String>)>,
    focused: bool,
}

#[derive(Clone, Copy, Debug)]
enum CustomIconTarget {
    Pane(Uuid),
    Tab(Uuid),
}

#[derive(Clone, Copy, Debug)]
struct SidebarPaneRowContext {
    workspace_id: Uuid,
    tab_id: Option<Uuid>,
    tab_color: Option<AppearanceColor>,
    from_group: bool,
    indent: f32,
}
fn detected_project_icon(project_dir: &Path) -> Option<PathBuf> {
    const ROOTS: [&str; 4] = ["", "public", "assets", "static"];
    const FILES: [&str; 8] = [
        "icon.png",
        "logo.png",
        "favicon.png",
        "apple-touch-icon.png",
        "icon.jpg",
        "logo.jpg",
        "icon.webp",
        "logo.webp",
    ];
    for root in ROOTS {
        for file in FILES {
            let candidate = project_dir.join(root).join(file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
fn remote_completion_parts(value: &str) -> (String, String) {
    value.rfind('/').map_or_else(
        || ("/".to_owned(), value.to_owned()),
        |slash| {
            (
                value[..=slash].to_owned(),
                value[slash.saturating_add(1)..].to_owned(),
            )
        },
    )
}

fn longest_common_prefix(values: &[String]) -> String {
    let Some(first) = values.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for value in &values[1..] {
        while !value.starts_with(&prefix) {
            if prefix.pop().is_none() {
                return prefix;
            }
        }
    }
    prefix
}

// These flags represent independent UI interactions, not one state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct HhApp {
    focus_handle: FocusHandle,
    /// Screen traffic only: pane updates, targeted pane snapshots, history
    /// status. Kept separate so a keystroke never waits behind a screen payload.
    stream_client: SharedSessionClient,
    /// Everything else, including terminal input and selection updates.
    control_client: SharedSessionClient,
    terminal_font: TerminalFontProfile,
    terminal_zoom_levels: HashMap<Uuid, i8>,
    keymap: ResolvedKeymap,
    snapshot: Option<SessionSnapshot>,
    screens: HashMap<Uuid, TerminalScreen>,
    pane_states: HashMap<Uuid, PaneStreamState>,
    notifications: Vec<SessionNotification>,
    notifications_latest_id: u64,
    /// When each pane's screen was last applied, used to pace on-screen panes
    /// other than the focused one.
    last_delivery: HashMap<Uuid, Instant>,
    /// Last time output was delivered or the user acted, driving the deep-idle
    /// polling tier.
    window_active: bool,
    last_activity: Instant,
    stream_diagnostics: StreamDiagnostics,
    active_workspace: Option<Uuid>,
    workspace_tab_scope: WorkspaceTabScope,
    expanded_workspaces: HashSet<Uuid>,
    collapsed_groups: HashSet<Uuid>,
    collapsed_pinned_sections: HashSet<Uuid>,
    collapsed_project_sections: HashSet<Uuid>,
    dismissed_workspace_tabs: HashSet<Uuid>,
    focused_pane: Option<Uuid>,
    split_ratios: HashMap<SplitControlId, f32>,
    zoomed_pane: Option<Uuid>,
    modal: Modal,
    resizing: Option<ResizeDrag>,
    sidebar_resize: SidebarResizeLifecycle,
    ui_state_store: Option<UiStateStore>,
    preferred_sidebar_width: f32,
    sidebar_visible: bool,
    sidebar_activity: bool,
    sidebar_pixels: f32,
    workstation_tab_scroll: ScrollHandle,
    dragging_workspace: Option<Uuid>,
    workspace_drop_preview: Option<WorkspaceDropPreview>,
    suppress_workspace_click_until: Option<Instant>,
    tab_drop_preview: Option<TabDropPreview>,
    suppress_tab_click_until: Option<Instant>,
    last_sizes: HashMap<Uuid, (u16, u16)>,
    resize_generation: u64,
    workspace_pixels: (f32, f32),
    connection_error: Option<String>,
    history_status: Option<HistoryArchiveStatus>,
    archived_views: HashMap<Uuid, ArchivedView>,
    history_editor: Option<HistoryEditor>,
    history_clear_confirmation: Option<HistoryClearScope>,
    color_picker: Option<ColorPickerState>,
    custom_icons: Vec<CustomIcon>,
    workstation_banner: Option<BannerArtwork>,
    workstation_banner_hidden: bool,
    dragging_pane: Option<Uuid>,
    drag_hover: DragHoverState,
    browser_url_editor: Option<BrowserUrlEditor>,
    #[cfg(all(target_os = "macos", feature = "browser"))]
    browser_views: HashMap<Uuid, BrowserPaneView>,
    #[cfg(all(target_os = "macos", feature = "browser"))]
    browser_parent_view: Option<*mut c_void>,
    #[cfg(all(target_os = "macos", feature = "browser"))]
    browser_runtime_initialized: bool,
    #[cfg(all(target_os = "macos", feature = "browser"))]
    browser_runtime_error: Option<String>,
    #[cfg(all(target_os = "macos", feature = "browser"))]
    cef_shutdown_subscription: Option<gpui::Subscription>,
    selection_drag: Option<SelectionDrag>,
    ime_preedit: String,
    workspace_input_focus: [FocusHandle; 2],
    workspace_input_layouts: [Option<ShapedLine>; 2],
    workspace_input_bounds: [Option<Bounds<Pixels>>; 2],
}

impl HhApp {
    fn new(window: &mut Window, keymap: ResolvedKeymap, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        let workspace_input_focus = [cx.focus_handle(), cx.focus_handle()];
        let terminal_font = TerminalFontProfile::resolve(cx.text_system());
        let ui_state_store = match UiStateStore::from_default_path() {
            Ok(store) => Some(store),
            Err(error) => {
                eprintln!("Harness Harlot UI state unavailable: {error:#}");
                None
            }
        };
        let stored_sidebar_width =
            ui_state_store
                .as_ref()
                .and_then(|store| match store.load_workspace_sidebar_width() {
                    Ok(width) => width,
                    Err(error) => {
                        eprintln!("Harness Harlot UI state ignored: {error:#}");
                        None
                    }
                });
        let preferred_sidebar_width = migrated_sidebar_width(stored_sidebar_width);
        if development_build()
            && stored_sidebar_width != Some(preferred_sidebar_width)
            && let Some(store) = &ui_state_store
            && let Err(error) = store.save_workspace_sidebar_width(preferred_sidebar_width)
        {
            eprintln!("Harness Harlot sidebar default correction was not persisted: {error:#}");
        }
        let workstation_banner =
            ui_state_store
                .as_ref()
                .and_then(|store| match store.load_workstation_banner() {
                    Ok(stored) => stored.map(|stored| BannerArtwork {
                        image: Arc::new(Image::from_bytes(ImageFormat::Png, stored.png)),
                        width: stored.width,
                        height: stored.height,
                    }),
                    Err(error) => {
                        eprintln!("Harness Harlot custom workstation banner ignored: {error:#}");
                        None
                    }
                });
        let workstation_banner_hidden = ui_state_store.as_ref().is_some_and(|store| {
            match store.load_workstation_banner_hidden() {
                Ok(hidden) => hidden,
                Err(error) => {
                    eprintln!("Harness Harlot banner visibility ignored: {error:#}");
                    false
                }
            }
        });
        let stream_client = Arc::new(Mutex::new(SessionClient::connect().ok()));
        let control_client = Arc::new(Mutex::new(SessionClient::connect().ok()));
        #[cfg(all(target_os = "macos", feature = "browser"))]
        let browser_parent_view = native_nsview(window);
        let mut app = Self {
            focus_handle,
            stream_client,
            control_client,
            terminal_font,
            terminal_zoom_levels: HashMap::new(),
            keymap,
            snapshot: None,
            screens: HashMap::new(),
            pane_states: HashMap::new(),
            notifications: Vec::new(),
            notifications_latest_id: 0,
            last_delivery: HashMap::new(),
            last_activity: Instant::now(),
            stream_diagnostics: StreamDiagnostics::default(),
            active_workspace: None,
            workspace_tab_scope: WorkspaceTabScope::Workstation,
            expanded_workspaces: HashSet::new(),
            window_active: window.is_window_active(),
            collapsed_groups: HashSet::new(),
            collapsed_pinned_sections: HashSet::new(),
            collapsed_project_sections: HashSet::new(),
            dismissed_workspace_tabs: HashSet::new(),
            focused_pane: None,
            split_ratios: HashMap::new(),
            zoomed_pane: None,
            modal: Modal::None,
            resizing: None,
            sidebar_resize: SidebarResizeLifecycle::default(),
            ui_state_store,
            preferred_sidebar_width,
            sidebar_visible: true,
            sidebar_activity: false,
            sidebar_pixels: default_sidebar_width(),
            workstation_tab_scroll: ScrollHandle::new(),
            dragging_workspace: None,
            workspace_drop_preview: None,
            suppress_workspace_click_until: None,
            tab_drop_preview: None,
            suppress_tab_click_until: None,
            last_sizes: HashMap::new(),
            resize_generation: 0,
            workspace_pixels: (0.0, 0.0),
            connection_error: None,
            history_status: None,
            archived_views: HashMap::new(),
            history_editor: None,
            history_clear_confirmation: None,
            color_picker: None,
            custom_icons: load_custom_icons(),
            workstation_banner,
            workstation_banner_hidden,
            dragging_pane: None,
            drag_hover: DragHoverState::default(),
            browser_url_editor: None,
            #[cfg(all(target_os = "macos", feature = "browser"))]
            browser_views: HashMap::new(),
            #[cfg(all(target_os = "macos", feature = "browser"))]
            browser_parent_view,
            #[cfg(all(target_os = "macos", feature = "browser"))]
            browser_runtime_initialized: false,
            #[cfg(all(target_os = "macos", feature = "browser"))]
            browser_runtime_error: None,
            #[cfg(all(target_os = "macos", feature = "browser"))]
            cef_shutdown_subscription: None,
            selection_drag: None,
            ime_preedit: String::new(),
            workspace_input_focus,
            workspace_input_layouts: [None, None],
            workspace_input_bounds: [None, None],
        };
        app.update_window_geometry(window);
        app.refresh_state();
        app.refresh_notifications();
        if app.focused_pane.is_some() && app.screens.is_empty() {
            app.refresh_state();
        }

        #[cfg(all(target_os = "macos", feature = "browser"))]
        {
            app.cef_shutdown_subscription = Some(cx.on_app_quit(|this, _| {
                this.browser_views.clear();
                async {
                    hh_cef_view::shutdown_runtime();
                }
            }));
        }

        cx.observe_window_bounds(window, |this, window, cx| {
            if this.update_window_geometry(window) {
                this.sync_pty_sizes(cx);
                cx.notify();
            }
        })
        .detach();

        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if this.window_active {
                if let Some(pane_id) = this.focused_pane
                    && this.auto_read_pane_notifications(pane_id)
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
                gpui::Timer::after(Duration::from_millis(poll_delay_ms)).await;
                let Ok((update_request, client)) = this.update(cx, |this, _| {
                    (this.pane_update_request(), Arc::clone(&this.stream_client))
                }) else {
                    break;
                };
                let response = cx
                    .background_spawn(async move { session_call(&client, &update_request) })
                    .await;
                let Ok((state_changed, deep_idle)) = this.update(cx, |this, cx| {
                    let state_changed = this.apply_update_result(response);
                    if state_changed {
                        this.last_activity = Instant::now();
                    }
                    let deep_idle = Instant::now().saturating_duration_since(this.last_activity)
                        >= DEEP_IDLE_AFTER;
                    this.sync_pty_sizes(cx);
                    this.flush_browser_state_updates(cx);
                    if state_changed {
                        cx.notify();
                    }
                    (state_changed, deep_idle)
                }) else {
                    break;
                };
                poll_delay_ms =
                    next_terminal_poll_delay_ms(poll_delay_ms, state_changed, deep_idle);
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_secs(5)).await;
                let Ok(client) = this.update(cx, |this, _| Arc::clone(&this.stream_client)) else {
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
        app
    }
    /// Commands and terminal input: never blocked behind a screen payload.
    fn call(&self, request: &ClientRequest) -> anyhow::Result<ServiceResponse> {
        session_call(&self.control_client, request)
    }

    /// Screen traffic: pane updates, targeted pane snapshots, history status.
    fn stream_call(&self, request: &ClientRequest) -> anyhow::Result<ServiceResponse> {
        session_call(&self.stream_client, request)
    }

    fn notify(&self, request: &ClientRequest) -> anyhow::Result<()> {
        session_notify(&self.control_client, request)
    }

    fn report(&mut self, error: &anyhow::Error) {
        self.connection_error = Some(format!("{error:#}"));
    }

    fn report_unexpected(&mut self, response: &ServiceResponse) {
        self.connection_error = Some(format!("unexpected response: {response:?}"));
    }

    fn refresh_state(&mut self) -> bool {
        self.apply_update_result(self.stream_call(&self.pane_update_request()))
    }

    fn pane_update_request(&self) -> ClientRequest {
        let now = Instant::now();
        let pane_revisions = self
            .screens
            .values()
            .map(|screen| PaneRevisionCursor {
                pane_id: screen.pane_id,
                revision: screen.revision,
            })
            .collect();
        let subscribed_panes = paced_subscriptions(
            now,
            &self.on_screen_panes(),
            self.focused_pane,
            &self.last_delivery,
            SECONDARY_PANE_INTERVAL,
        );
        ClientRequest::GetUpdates {
            snapshot_revision: self.snapshot.as_ref().map(|snapshot| snapshot.revision),
            pane_revisions,
            subscribed_panes,
            notifications_after: self.notifications_latest_id,
        }
    }

    fn apply_update_result(&mut self, result: anyhow::Result<ServiceResponse>) -> bool {
        let state_changed = match result {
            Ok(ServiceResponse::Updates {
                session_revision,
                snapshot,
                screens,
                pane_states,
                notifications: notification_deltas,
                diagnostics,
            }) => {
                let apply_started = Instant::now();
                let current_session_revision =
                    self.snapshot.as_ref().map(|snapshot| snapshot.revision);
                let topology_is_current =
                    current_session_revision.is_none_or(|current| session_revision >= current);
                let mut snapshot_changed = false;
                let mut screens_applied = 0;
                let mut focus_resync = None;
                if let Some(snapshot) = snapshot
                    && current_session_revision.is_none_or(|current| snapshot.revision >= current)
                {
                    snapshot_changed = self.snapshot.as_ref() != Some(&snapshot);
                    if self.active_workspace.is_none()
                        || !snapshot.workspaces.iter().any(|workspace| {
                            Some(workspace.id) == self.active_workspace
                                && workspace_is_selectable(workspace)
                        })
                    {
                        self.active_workspace = snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace_is_selectable(workspace))
                            .map(|workspace| workspace.id);
                    }
                    let visible = self
                        .active_workspace_in(&snapshot)
                        .map(workspace_visible_panes)
                        .unwrap_or_default();
                    if self
                        .zoomed_pane
                        .is_some_and(|pane| !visible.contains(&pane))
                    {
                        self.zoomed_pane = None;
                        self.last_sizes.clear();
                    }
                    let focused_exists = self.focused_pane.is_some_and(|pane_id| {
                        self.active_workspace_in(&snapshot)
                            .is_some_and(|workspace| {
                                workspace
                                    .tabs
                                    .iter()
                                    .any(|tab| find_pane(&tab.layout, pane_id).is_some())
                            })
                    });
                    match focus_resync_for(&visible, self.focused_pane, focused_exists) {
                        FocusResync::Keep => {}
                        FocusResync::Reassert(pane_id) => {
                            // Never nest `refresh_state` while applying a snapshot: the
                            // outer stale snapshot would overwrite the nested fresh one.
                            self.send_control(&ClientRequest::ActivateTab { pane_id });
                        }
                        FocusResync::Switch(pane_id) => focus_resync = Some(pane_id),
                        FocusResync::Clear => self.focused_pane = None,
                    }
                    let live_tab_ids = snapshot
                        .workspaces
                        .iter()
                        .flat_map(|workspace| workspace.tabs.iter().map(|tab| tab.id))
                        .collect::<HashSet<_>>();
                    self.collapsed_groups
                        .retain(|tab_id| live_tab_ids.contains(tab_id));
                    self.dismissed_workspace_tabs
                        .retain(|tab_id| live_tab_ids.contains(tab_id));
                    self.snapshot = Some(snapshot);
                }
                let delivered_at = Instant::now();
                for screen in screens {
                    let is_newer = self
                        .screens
                        .get(&screen.pane_id)
                        .is_none_or(|current| screen.revision > current.revision);
                    if is_newer {
                        self.last_delivery.insert(screen.pane_id, delivered_at);
                        self.screens.insert(screen.pane_id, screen);
                        screens_applied += 1;
                    }
                }
                if topology_is_current {
                    let live_panes = pane_states
                        .iter()
                        .map(|state| state.pane_id)
                        .collect::<std::collections::HashSet<_>>();
                    self.screens
                        .retain(|pane_id, _| live_panes.contains(pane_id));
                    self.last_delivery
                        .retain(|pane_id, _| live_panes.contains(pane_id));
                    self.split_ratios.retain(|id, _| {
                        live_panes.contains(&id.first) && live_panes.contains(&id.second)
                    });
                    self.terminal_zoom_levels
                        .retain(|pane_id, _| live_panes.contains(pane_id));
                    self.pane_states = pane_states
                        .into_iter()
                        .map(|state| (state.pane_id, state))
                        .collect();
                }
                let notifications_changed = if notification_deltas
                    .iter()
                    .any(|notification| notification.id <= self.notifications_latest_id)
                {
                    self.notifications.clear();
                    self.notifications_latest_id = 0;
                    self.refresh_notifications()
                } else if notification_deltas.is_empty() {
                    false
                } else {
                    self.notifications_latest_id = notification_deltas
                        .iter()
                        .map(|notification| notification.id)
                        .max()
                        .unwrap_or(self.notifications_latest_id);
                    self.notifications.extend(notification_deltas);
                    let overflow = self.notifications.len().saturating_sub(200);
                    if overflow > 0 {
                        self.notifications.drain(..overflow);
                    }
                    if self.window_active
                        && let Some(pane_id) = self.focused_pane
                    {
                        self.auto_read_pane_notifications(pane_id);
                    } else {
                        self.sync_dock_badge();
                    }
                    true
                };
                self.stream_diagnostics = diagnostics;
                let connection_changed = self.connection_error.take().is_some();
                self.connection_error = None;
                let mut state_changed =
                    pane_update_requires_repaint(snapshot_changed, screens_applied)
                        || connection_changed
                        || notifications_changed;
                if let Some(pane_id) = focus_resync {
                    state_changed |= self.focus_pane_with_snapshot(pane_id);
                }
                self.stream_diagnostics.desktop_apply_micros =
                    u64::try_from(apply_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                state_changed
            }
            Ok(response) => {
                let previous = self.connection_error.clone();
                self.report_unexpected(&response);
                self.connection_error != previous
            }
            Err(error) => {
                let previous = self.connection_error.clone();
                self.report(&error);
                self.connection_error != previous
            }
        };
        state_changed | self.sync_browser_callback_state()
    }

    fn focus_pane_with_snapshot(&mut self, pane_id: Uuid) -> bool {
        let needs_activation = self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.workspaces.iter().any(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|tab| inactive_stack_contains(&tab.layout, pane_id))
            })
        });
        if needs_activation {
            self.send(&ClientRequest::ActivateTab { pane_id });
        }
        let notifications_changed = self.auto_read_pane_notifications(pane_id);
        if self
            .pane_metadata(pane_id)
            .is_some_and(|pane| matches!(pane.kind, PaneKind::Browser { .. }))
        {
            let changed = self.focused_pane != Some(pane_id);
            self.focused_pane = Some(pane_id);
            self.connection_error = None;
            return changed || notifications_changed;
        }
        if self.focused_pane == Some(pane_id) {
            return notifications_changed;
        }
        match self.stream_call(&ClientRequest::GetPaneSnapshot { pane_id }) {
            Ok(ServiceResponse::PaneSnapshot {
                screen,
                diagnostics,
            }) => {
                let delivered_at = Instant::now();
                let changed = self.focused_pane != Some(pane_id)
                    || self
                        .screens
                        .get(&pane_id)
                        .is_none_or(|current| current.revision != screen.revision);
                self.pane_states.insert(
                    pane_id,
                    PaneStreamState {
                        pane_id,
                        revision: screen.revision,
                        subscribed: true,
                        dirty: false,
                        // A focus snapshot says nothing about liveness; keep
                        // whatever the last update round reported.
                        exited: self
                            .pane_states
                            .get(&pane_id)
                            .is_some_and(|state| state.exited),
                    },
                );
                self.screens.insert(pane_id, screen);
                self.focused_pane = Some(pane_id);
                self.last_delivery.insert(pane_id, delivered_at);
                self.stream_diagnostics = diagnostics;
                self.connection_error = None;
                changed || notifications_changed
            }
            Ok(response) => {
                self.report_unexpected(&response);
                notifications_changed
            }
            Err(error) => {
                self.report(&error);
                notifications_changed
            }
        }
    }

    fn active_workspace_in<'a>(&self, snapshot: &'a SessionSnapshot) -> Option<&'a Workspace> {
        let active = self.active_workspace?;
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == active)
    }

    /// The layout the viewport is actually rendering: the tab holding the
    /// focused pane, not blindly the workstation's first tab. Sizing, zoom,
    /// and split geometry must agree with what is on screen.
    fn active_layout<'a>(&self, snapshot: &'a SessionSnapshot) -> Option<&'a PaneLayout> {
        self.active_workspace_in(snapshot)
            .and_then(|workspace| workspace_layout_for_focused_pane(workspace, self.focused_pane))
    }

    /// Panes rendered right now: the tab holding the focused pane, zoom
    /// applied. This is the same projection `sync_pty_sizes` resizes, so what
    /// is sized is exactly what streams.
    fn on_screen_panes(&self) -> Vec<Uuid> {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Vec::new();
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return Vec::new();
        };
        let projected = self
            .zoomed_pane
            .and_then(|pane_id| zoom_projection(layout, pane_id));
        visible_panes(projected.as_ref().unwrap_or(layout))
    }

    fn terminal_accent(&self, pane_id: Uuid) -> AppearanceColor {
        self.snapshot
            .as_ref()
            .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                resolved_terminal_accent(snapshot, pane_id)
            })
    }

    fn workspace_color(&self, workspace_id: Uuid) -> AppearanceColor {
        self.snapshot
            .as_ref()
            .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                resolved_workspace_color(snapshot, workspace_id)
            })
    }

    fn appearance_choices(&self) -> Vec<AppearanceColor> {
        let mut colors = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.appearance.recent_colors.clone())
            .unwrap_or_default();
        for preset in APPEARANCE_PRESETS {
            if !colors.contains(&preset) {
                colors.push(preset);
            }
        }
        colors.truncate(12);
        colors
    }

    fn color_for_target(&self, target: ColorTarget) -> AppearanceColor {
        match target {
            ColorTarget::DefaultTerminal => self
                .snapshot
                .as_ref()
                .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                    snapshot.appearance.default_terminal_accent
                }),
            ColorTarget::DefaultWorkspace => self
                .snapshot
                .as_ref()
                .map_or(AppearanceColor::DARK_GRAY, |snapshot| {
                    snapshot.appearance.default_workspace_color
                }),
            ColorTarget::Pane(pane_id) => self.terminal_accent(pane_id),
            ColorTarget::Workspace(workspace_id) => self.workspace_color(workspace_id),
            ColorTarget::Tab(tab_id) => self
                .snapshot
                .as_ref()
                .and_then(|snapshot| {
                    snapshot
                        .workspaces
                        .iter()
                        .flat_map(|workspace| workspace.tabs.iter())
                        .find(|tab| tab.id == tab_id)
                        .and_then(|tab| tab.color)
                        .or(Some(snapshot.appearance.default_workspace_color))
                })
                .unwrap_or(AppearanceColor::DARK_GRAY),
        }
    }

    fn apply_color(
        &mut self,
        target: ColorTarget,
        color: Option<AppearanceColor>,
        cx: &mut Context<Self>,
    ) {
        let request = match (target, color) {
            (ColorTarget::DefaultTerminal, Some(color)) => {
                ClientRequest::SetDefaultTerminalAccent { color }
            }
            (ColorTarget::DefaultWorkspace, Some(color)) => {
                ClientRequest::SetDefaultWorkspaceColor { color }
            }
            (ColorTarget::Pane(pane_id), color) => ClientRequest::SetPaneColor { pane_id, color },
            (ColorTarget::Workspace(workspace_id), color) => ClientRequest::SetWorkspaceColor {
                workspace_id,
                color,
            },
            (ColorTarget::Tab(tab_id), color) => ClientRequest::SetTabColor { tab_id, color },
            (ColorTarget::DefaultTerminal | ColorTarget::DefaultWorkspace, None) => return,
        };
        self.send(&request);
        if matches!(
            self.modal,
            Modal::TabMenu(_) | Modal::WorkspaceMenu(_) | Modal::GroupMenu(_)
        ) {
            self.modal = Modal::None;
        }
        self.color_picker = None;
        cx.notify();
    }

    fn open_color_picker(&mut self, target: ColorTarget, cx: &mut Context<Self>) {
        let current = self.color_for_target(target).as_rgb();
        self.color_picker = Some(ColorPickerState {
            target,
            hex: format!("{current:06X}"),
            replace_on_type: true,
            invalid: false,
        });
        if !matches!(
            target,
            ColorTarget::Pane(_) | ColorTarget::Workspace(_) | ColorTarget::Tab(_)
        ) {
            self.modal = Modal::None;
        }
        cx.notify();
    }

    fn submit_color_picker(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.color_picker.as_ref() else {
            return;
        };
        let target = picker.target;
        let color = parse_hex_color(&picker.hex);
        if let Some(color) = color {
            self.apply_color(target, Some(color), cx);
        } else if let Some(picker) = self.color_picker.as_mut() {
            picker.invalid = true;
            cx.notify();
        }
    }

    fn open_appearance_settings(&mut self, cx: &mut Context<Self>) {
        self.modal = Modal::AppearanceSettings;
        self.color_picker = None;
        self.history_editor = None;
        self.history_clear_confirmation = None;
        let _ = self.refresh_history_status();
        cx.notify();
    }

    fn refresh_notifications(&mut self) -> bool {
        let previous = self.notifications.clone();
        match self.call(&ClientRequest::GetNotifications) {
            Ok(ServiceResponse::Notifications { items }) => {
                self.notifications_latest_id =
                    items.last().map_or(0, |notification| notification.id);
                self.notifications = items;
                self.connection_error = None;
                self.sync_dock_badge();
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.notifications != previous
    }

    fn unread_notification_count(&self) -> usize {
        self.notifications
            .iter()
            .filter(|notification| !notification.read)
            .count()
    }

    fn sync_dock_badge(&self) {
        let unread = self.unread_notification_count();
        if unread == 0 {
            set_macos_dock_badge(None);
        } else {
            let label = unread.to_string();
            set_macos_dock_badge(Some(&label));
        }
    }

    fn mark_notification_ids_read(&mut self, ids: &[u64]) -> bool {
        if ids.is_empty() {
            return false;
        }
        match self.call(&ClientRequest::MarkNotificationsRead { ids: ids.to_vec() }) {
            Ok(ServiceResponse::Ack) => {
                let ids = ids.iter().copied().collect::<HashSet<_>>();
                for notification in &mut self.notifications {
                    if ids.contains(&notification.id) {
                        notification.read = true;
                    }
                }
                self.connection_error = None;
                self.sync_dock_badge();
                true
            }
            Ok(response) => {
                self.report_unexpected(&response);
                false
            }
            Err(error) => {
                self.report(&error);
                false
            }
        }
    }

    fn auto_read_pane_notifications(&mut self, pane_id: Uuid) -> bool {
        let ids = self
            .notifications
            .iter()
            .filter(|notification| notification.pane_id == pane_id && !notification.read)
            .map(|notification| notification.id)
            .collect::<Vec<_>>();
        self.mark_notification_ids_read(&ids)
    }

    fn mark_all_notifications_read(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .notifications
            .iter()
            .filter(|notification| !notification.read)
            .map(|notification| notification.id)
            .collect::<Vec<_>>();
        if self.mark_notification_ids_read(&ids) {
            cx.notify();
        }
    }

    fn clear_notifications(&mut self, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::ClearNotifications) {
            Ok(ServiceResponse::Ack) => {
                self.notifications.clear();
                self.connection_error = None;
                self.sync_dock_badge();
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn open_notification(
        &mut self,
        notification_id: u64,
        pane_id: Uuid,
        workspace_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let pane_exists = self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .is_some_and(|workspace| {
                    workspace
                        .tabs
                        .iter()
                        .any(|tab| find_pane(&tab.layout, pane_id).is_some())
                })
        });
        if pane_exists {
            self.select_workspace_tab(workspace_id, pane_id, cx);
            self.modal = Modal::None;
        } else {
            self.mark_notification_ids_read(&[notification_id]);
        }
        cx.notify();
    }

    fn refresh_history_status(&mut self) -> bool {
        let response = self.call(&ClientRequest::GetHistoryStatus);
        self.apply_history_status_result(response)
    }

    fn apply_history_status_result(&mut self, response: anyhow::Result<ServiceResponse>) -> bool {
        let previous = self.history_status.clone();
        match response {
            Ok(ServiceResponse::HistoryStatus { status }) => {
                self.history_status = Some(status);
                self.connection_error = None;
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        self.history_status != previous
    }

    fn apply_history_settings(&mut self, settings: HistorySettings, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::SetHistorySettings { settings }) {
            Ok(ServiceResponse::HistoryStatus { status }) => {
                self.history_status = Some(status);
                self.history_editor = None;
                self.connection_error = None;
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn mutate_history_settings(
        &mut self,
        update: impl FnOnce(&mut HistorySettings),
        cx: &mut Context<Self>,
    ) {
        let Some(mut settings) = self
            .history_status
            .as_ref()
            .map(|status| status.settings.clone())
        else {
            let _ = self.refresh_history_status();
            cx.notify();
            return;
        };
        update(&mut settings);
        self.apply_history_settings(settings, cx);
    }
    fn clear_history(&mut self, scope: HistoryClearScope, cx: &mut Context<Self>) {
        if self.history_clear_confirmation != Some(scope) {
            self.history_clear_confirmation = Some(scope);
            cx.notify();
            return;
        }
        match self.call(&ClientRequest::ClearHistory { scope }) {
            Ok(ServiceResponse::HistoryStatus { status }) => {
                self.history_status = Some(status);
                self.history_clear_confirmation = None;
                self.archived_views.clear();
                self.connection_error = None;
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn begin_history_edit(&mut self, field: HistoryEditField, cx: &mut Context<Self>) {
        let text = match (field, self.history_status.as_ref()) {
            (
                HistoryEditField::RetentionDays,
                Some(HistoryArchiveStatus {
                    settings:
                        HistorySettings {
                            retention: HistoryRetention::Days { days },
                            ..
                        },
                    ..
                }),
            ) => days.to_string(),
            (HistoryEditField::RetentionDays, _) => "30".to_owned(),
            (HistoryEditField::QuotaGib, Some(status)) => {
                (status.settings.quota_bytes / 1024 / 1024 / 1024).to_string()
            }
            (HistoryEditField::QuotaGib, None) => "5".to_owned(),
        };
        self.history_editor = Some(HistoryEditor {
            field,
            text,
            replace_on_type: true,
            invalid: false,
        });
        cx.notify();
    }

    fn submit_history_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.history_editor.as_ref() else {
            return;
        };
        let field = editor.field;
        let Ok(value) = editor.text.parse::<u64>() else {
            if let Some(editor) = self.history_editor.as_mut() {
                editor.invalid = true;
            }
            cx.notify();
            return;
        };
        match field {
            HistoryEditField::RetentionDays if (1..=3_650).contains(&value) => {
                self.mutate_history_settings(
                    |settings| {
                        settings.retention = HistoryRetention::Days {
                            days: u32::try_from(value).unwrap_or(3_650),
                        };
                    },
                    cx,
                );
            }
            HistoryEditField::QuotaGib if (1..=4_096).contains(&value) => {
                self.mutate_history_settings(
                    |settings| {
                        settings.quota_bytes = value * 1024 * 1024 * 1024;
                    },
                    cx,
                );
            }
            _ => {
                if let Some(editor) = self.history_editor.as_mut() {
                    editor.invalid = true;
                }
                cx.notify();
            }
        }
    }

    fn send(&mut self, request_message: &ClientRequest) {
        self.send_control(request_message);
        self.refresh_state();
    }

    fn send_control(&mut self, request_message: &ClientRequest) {
        self.last_activity = Instant::now();
        let result = if matches!(
            request_message,
            ClientRequest::WriteInput { .. } | ClientRequest::UpdateSelection { .. }
        ) {
            self.notify(request_message)
        } else {
            self.call(request_message).map(|_| ())
        };
        if let Err(error) = result {
            self.report(&error);
        }
    }

    fn new_workspace(&mut self, cx: &mut Context<Self>) {
        self.begin_workspace_creation(cx);
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        if self.sidebar_resize.finish() {
            self.persist_sidebar_width(cx);
        }
        let window_width = self.workspace_pixels.0 + self.sidebar_pixels;
        self.sidebar_pixels = sidebar_width_for_visibility(
            self.preferred_sidebar_width,
            window_width,
            self.sidebar_visible,
        );
        self.workspace_pixels.0 = (window_width - self.sidebar_pixels).max(1.0);
        self.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    fn toggle_sidebar_activity(&mut self, cx: &mut Context<Self>) {
        self.sidebar_activity = !self.sidebar_activity;
        if self.sidebar_activity {
            self.sidebar_visible = true;
        }
        self.refresh_notifications();
        cx.notify();
    }

    fn toggle_workspace_expanded(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        if !self.expanded_workspaces.remove(&workspace_id) {
            self.expanded_workspaces.insert(workspace_id);
        }
        cx.notify();
    }

    fn toggle_group_collapsed(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(&tab_id) {
            self.collapsed_groups.insert(tab_id);
        }
        cx.notify();
    }

    fn toggle_sidebar_section(
        &mut self,
        workspace_id: Uuid,
        section: SidebarSection,
        cx: &mut Context<Self>,
    ) {
        let collapsed = match section {
            SidebarSection::Pinned => &mut self.collapsed_pinned_sections,
            SidebarSection::Projects => &mut self.collapsed_project_sections,
        };
        if !collapsed.remove(&workspace_id) {
            collapsed.insert(workspace_id);
        }
        cx.notify();
    }

    fn select_workspace(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.workspace_tab_scope = WorkspaceTabScope::Workstation;
        self.expanded_workspaces.insert(workspace_id);
        self.active_workspace = Some(workspace_id);
        let first_pane = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .and_then(|workspace| workspace.tabs.first())
                .and_then(|tab| visible_panes(&tab.layout).first().copied())
        });
        if let Some(pane_id) = first_pane {
            self.focus_pane_with_snapshot(pane_id);
        }
        self.last_sizes.clear();
        cx.notify();
    }

    fn select_workspace_by_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let mut workspace_ids = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.workspaces.clone())
            .unwrap_or_default();
        workspace_ids.sort_by_key(|workspace| {
            (
                !workspace.pinned,
                if workspace.pinned {
                    workspace.pin_order
                } else {
                    u32::MAX
                },
            )
        });
        let Some(workspace_id) = workspace_ids.get(index).map(|workspace| workspace.id) else {
            return false;
        };
        self.select_workspace(workspace_id, cx);
        self.workstation_tab_scroll.scroll_to_item(index);
        true
    }

    fn select_workspace_tab(&mut self, workspace_id: Uuid, pane_id: Uuid, cx: &mut Context<Self>) {
        let switched_workspace = self.active_workspace != Some(workspace_id);
        let selected_tab_id = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .and_then(|workspace| {
                    workspace
                        .tabs
                        .iter()
                        .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                        .map(|tab| tab.id)
                })
        });
        match self.call(&ClientRequest::ActivateTab { pane_id }) {
            Ok(ServiceResponse::Ack) => {
                self.refresh_state();
                self.active_workspace = Some(workspace_id);
                if switched_workspace {
                    self.workspace_tab_scope = WorkspaceTabScope::Workstation;
                }
                let refreshed_pane_id = selected_tab_id.map_or(Some(pane_id), |tab_id| {
                    self.snapshot.as_ref().and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.id == workspace_id)
                            .and_then(|workspace| {
                                workspace_tab_click_target(workspace, tab_id, Some(pane_id))
                            })
                    })
                });
                if let Some(refreshed_pane_id) = refreshed_pane_id {
                    self.focus_pane_with_snapshot(refreshed_pane_id);
                }
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }
    fn select_workspace_tab_root(
        &mut self,
        workspace_id: Uuid,
        tab_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .and_then(|workspace| {
                    workspace_tab_click_target(workspace, tab_id, self.focused_pane)
                })
        }) else {
            return;
        };
        self.select_workspace_tab(workspace_id, pane_id, cx);
    }

    fn dismiss_workspace_tab(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let standalone_pane = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .and_then(workspace_tab_standalone_pane)
                .map(|pane| pane.id)
        });
        if let Some(pane_id) = standalone_pane {
            self.begin_close(pane_id, cx);
        } else {
            self.dismissed_workspace_tabs.insert(tab_id);
            self.modal = Modal::None;
            cx.notify();
        }
    }

    fn select_sidebar_pane(
        &mut self,
        workspace_id: Uuid,
        tab_id: Uuid,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let scope = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == workspace_id)
            })
            .map_or(WorkspaceTabScope::Workstation, |workspace| {
                workspace_scope_for_tab(workspace, tab_id)
            });
        self.dismissed_workspace_tabs.remove(&tab_id);
        self.select_workspace_tab(workspace_id, pane_id, cx);
        self.workspace_tab_scope = scope;
        cx.notify();
    }

    fn new_tab(&mut self, cx: &mut Context<Self>) {
        let Some((workspace_id, scope, empty)) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| self.active_workspace_in(snapshot))
            .map(|workspace| {
                (
                    workspace.id,
                    workspace_tab_set(workspace, self.workspace_tab_scope).scope,
                    workspace.tabs.is_empty(),
                )
            })
        else {
            return;
        };
        if empty {
            self.open_workspace_terminal(workspace_id, cx);
            return;
        }
        match scope {
            WorkspaceTabScope::Workstation => self.new_workspace_tab(workspace_id, cx),
            WorkspaceTabScope::Project(project_id) => {
                self.new_project_group(workspace_id, project_id, cx);
            }
        }
    }

    fn focus_created_pane(&mut self, workspace_id: Uuid, pane_id: Uuid, cx: &mut Context<Self>) {
        self.refresh_state();
        self.expanded_workspaces.insert(workspace_id);
        self.active_workspace = Some(workspace_id);
        self.focus_pane_with_snapshot(pane_id);
        cx.notify();
    }

    fn focus_created_pane_inferred(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.refresh_state();
        if let Some(workspace_id) = self.workspace_id_for_pane(pane_id) {
            self.expanded_workspaces.insert(workspace_id);
            self.active_workspace = Some(workspace_id);
        }
        self.focus_pane_with_snapshot(pane_id);
        cx.notify();
    }

    fn open_workspace_terminal(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::CreateWorkspaceTerminal { workspace_id }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_created_pane(workspace_id, pane_id, cx);
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        cx.notify();
    }

    fn new_workspace_tab(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::CreateWorkspaceTab { workspace_id }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_created_pane(workspace_id, pane_id, cx);
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        self.modal = Modal::None;
        cx.notify();
    }

    fn new_browser_tab(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.active_workspace else {
            return;
        };
        self.new_browser_tab_in(workspace_id, cx);
    }
    fn workspace_id_for_pane(&self, pane_id: Uuid) -> Option<Uuid> {
        self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| {
                    workspace
                        .tabs
                        .iter()
                        .any(|tab| find_pane(&tab.layout, pane_id).is_some())
                })
                .map(|workspace| workspace.id)
        })
    }

    fn workspace_id_for_tab(&self, tab_id: Uuid) -> Option<Uuid> {
        self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
                .map(|workspace| workspace.id)
        })
    }
    fn browser_group_target(&self, workspace_id: Uuid) -> Option<Uuid> {
        let workspace = self
            .snapshot
            .as_ref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)?;
        self.focused_pane
            .filter(|pane_id| {
                workspace.tabs.iter().any(|tab| {
                    find_pane(&tab.layout, *pane_id)
                        .is_some_and(|pane| matches!(pane.kind, PaneKind::Terminal))
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
                    .find(|pane| matches!(pane.kind, PaneKind::Terminal))
                    .map(|pane| pane.id)
            })
    }

    fn create_browser(
        &mut self,
        workspace_id: Uuid,
        request: &ClientRequest,
        cx: &mut Context<Self>,
    ) {
        if !browser_command_available() {
            self.connection_error = Some("Browser tabs require the bundled macOS app".to_owned());
            self.modal = Modal::None;
            cx.notify();
            return;
        }
        match self.call(request) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.refresh_state();
                self.active_workspace = Some(workspace_id);
                self.focus_pane_with_snapshot(pane_id);
                self.expanded_workspaces.insert(workspace_id);
                self.browser_url_editor = Some(BrowserUrlEditor {
                    pane_id,
                    text: String::new(),
                    replace_on_type: true,
                    invalid: false,
                });
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        self.modal = Modal::None;
        cx.notify();
    }

    fn new_workspace_browser(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.create_browser(
            workspace_id,
            &ClientRequest::CreateBrowserTab {
                workspace_id,
                url: None,
            },
            cx,
        );
    }

    fn new_browser_tab_in(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
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
        self.create_browser(workspace_id, &request, cx);
    }

    fn tab_is_navigation_container(&self, tab_id: Uuid) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .is_some_and(|tab| workspace_tab_standalone_pane(tab).is_none())
        })
    }

    fn project_for_create_target(&self, tab_id: Uuid) -> Option<Uuid> {
        self.snapshot.as_ref().and_then(|snapshot| {
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

    fn add_terminal_to_context(
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

    fn add_browser_to_context(
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

    fn add_group_to_context(
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

    fn begin_browser_url_edit(&mut self, pane_id: Uuid, url: &str, cx: &mut Context<Self>) {
        self.browser_url_editor = Some(BrowserUrlEditor {
            pane_id,
            text: url.to_owned(),
            replace_on_type: true,
            invalid: false,
        });
        cx.notify();
    }

    fn append_browser_url_text(&mut self, text: &str) -> bool {
        let Some(editor) = self.browser_url_editor.as_mut() else {
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

    fn submit_browser_url(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.browser_url_editor.clone() else {
            return;
        };
        let Ok(url) = hh_protocol::normalize_browser_url(&editor.text) else {
            if let Some(editor) = self.browser_url_editor.as_mut() {
                editor.invalid = true;
            }
            cx.notify();
            return;
        };
        match self.call(&ClientRequest::SetBrowserState {
            pane_id: editor.pane_id,
            url: url.clone(),
            title: None,
        }) {
            Ok(ServiceResponse::Ack) => {
                #[cfg(all(target_os = "macos", feature = "browser"))]
                if let Some(view) = self.browser_views.get(&editor.pane_id) {
                    view.pane.navigate(&url);
                }
                self.browser_url_editor = None;
                self.refresh_state();
                self.connection_error = None;
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn browser_back(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        #[cfg(all(target_os = "macos", feature = "browser"))]
        if let Some(view) = self.browser_views.get(&pane_id) {
            view.pane.back();
        }
        #[cfg(not(all(target_os = "macos", feature = "browser")))]
        let _ = pane_id;
        cx.notify();
    }

    fn browser_forward(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        #[cfg(all(target_os = "macos", feature = "browser"))]
        if let Some(view) = self.browser_views.get(&pane_id) {
            view.pane.forward();
        }
        #[cfg(not(all(target_os = "macos", feature = "browser")))]
        let _ = pane_id;
        cx.notify();
    }

    fn browser_reload(&mut self, pane_id: Uuid, loading: bool, cx: &mut Context<Self>) {
        #[cfg(all(target_os = "macos", feature = "browser"))]
        if let Some(view) = self.browser_views.get(&pane_id) {
            if loading {
                view.pane.stop();
            } else {
                view.pane.reload();
            }
        }
        #[cfg(not(all(target_os = "macos", feature = "browser")))]
        let _ = (pane_id, loading);
        cx.notify();
    }

    fn new_workspace_group(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::CreateWorkspaceGroup {
            workspace_id,
            parent_tab: None,
        }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_created_pane(workspace_id, pane_id, cx);
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        self.modal = Modal::None;
        cx.notify();
    }

    fn new_tab_at(&mut self, target_pane: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::CreateGroupTerminal { target_pane }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_created_pane_inferred(pane_id, cx);
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn split(&mut self, axis: SplitAxis, cx: &mut Context<Self>) {
        if let Some(target_pane) = self.focused_pane {
            self.split_at(target_pane, axis, cx);
        }
    }

    fn split_at(&mut self, target_pane: Uuid, axis: SplitAxis, cx: &mut Context<Self>) {
        self.zoomed_pane = None;
        match self.call(&ClientRequest::CreatePane { target_pane, axis }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_created_pane_inferred(pane_id, cx);
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        cx.notify();
    }

    fn activate_tab(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::ActivateTab { pane_id }) {
            Ok(ServiceResponse::Ack) => {
                // Commit the click intent before applying any in-flight snapshot.
                // Otherwise a stale stack snapshot reasserts the previously
                // focused sibling and the clicked inner tab immediately reverts.
                self.focused_pane = Some(pane_id);
                self.refresh_state();
                self.focus_pane_with_snapshot(pane_id);
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    fn swap_panes(&mut self, source_pane: Uuid, target_pane: Uuid, cx: &mut Context<Self>) {
        if source_pane != target_pane {
            self.zoomed_pane = None;
            self.send(&ClientRequest::SwapPanes {
                source_pane,
                target_pane,
            });
            self.focus_pane_with_snapshot(source_pane);
            self.last_sizes.clear();
            cx.notify();
        }
    }

    fn move_pane_to_split(
        &mut self,
        source_pane: Uuid,
        target_pane: Uuid,
        placement: DropPlacement,
        cx: &mut Context<Self>,
    ) {
        self.dragging_pane = None;
        self.drag_hover.clear();
        self.zoomed_pane = None;
        self.send(&ClientRequest::MovePaneToSplit {
            source_pane,
            target_pane,
            placement,
        });
        self.focus_pane_with_snapshot(source_pane);
        self.last_sizes.clear();
        cx.notify();
    }

    fn move_pane_to_tab(&mut self, source_pane: Uuid, target_pane: Uuid, cx: &mut Context<Self>) {
        self.dragging_pane = None;
        self.drag_hover.clear();
        self.zoomed_pane = None;
        self.send(&ClientRequest::MovePaneToTab {
            source_pane,
            target_pane,
        });
        self.focus_pane_with_snapshot(source_pane);
        self.last_sizes.clear();
        cx.notify();
    }

    fn pane_metadata(&self, pane_id: Uuid) -> Option<Pane> {
        self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find_map(|tab| find_pane(&tab.layout, pane_id).cloned())
        })
    }

    fn group_metadata(&self, tab_id: Uuid) -> Option<(String, Uuid)> {
        self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| {
                    let pane_id = visible_panes(&tab.layout).first().copied().or_else(|| {
                        let mut panes = Vec::new();
                        collect_terminal_tabs(&tab.layout, &mut panes);
                        panes.first().map(|pane| pane.id)
                    })?;
                    Some((
                        tab.custom_title
                            .clone()
                            .unwrap_or_else(|| tab.title.clone()),
                        pane_id,
                    ))
                })
        })
    }

    fn open_tab_menu(&mut self, pane_id: Uuid, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Err(error) = self.call(&ClientRequest::ActivateTab { pane_id }) {
            self.report(&error);
        }
        self.focus_pane_with_snapshot(pane_id);
        self.refresh_state();
        self.modal = Modal::TabMenu(TabMenu {
            pane_id,
            position,
            identity_picker_open: false,
        });
        cx.notify();
    }

    fn open_workspace_menu(
        &mut self,
        workspace_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.modal = Modal::WorkspaceMenu(WorkspaceMenu {
            workspace_id,
            position,
        });
        self.last_sizes.clear();
        cx.notify();
    }

    fn open_group_menu(&mut self, tab_id: Uuid, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.modal = Modal::GroupMenu(GroupMenu {
            tab_id,
            position,
            icon_picker_open: false,
        });
        cx.notify();
    }

    fn new_group_terminal(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let target_pane = self.group_metadata(tab_id).map(|(_, pane_id)| pane_id);
        self.modal = Modal::None;
        if let Some(target_pane) = target_pane {
            self.new_tab_at(target_pane, cx);
        } else {
            cx.notify();
        }
    }

    fn new_group_browser(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let Some(target_pane) = self.group_metadata(tab_id).map(|(_, pane_id)| pane_id) else {
            self.modal = Modal::None;
            cx.notify();
            return;
        };
        match self.call(&ClientRequest::CreateGroupBrowser {
            target_pane,
            url: None,
        }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.refresh_state();
                self.focus_pane_with_snapshot(pane_id);
                self.browser_url_editor = Some(BrowserUrlEditor {
                    pane_id,
                    text: String::new(),
                    replace_on_type: true,
                    invalid: false,
                });
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.last_sizes.clear();
        self.modal = Modal::None;
        cx.notify();
    }

    fn new_project_group(&mut self, workspace_id: Uuid, tab_id: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::CreateWorkspaceGroup {
            workspace_id,
            parent_tab: Some(tab_id),
        }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.focus_created_pane(workspace_id, pane_id, cx);
                self.collapsed_groups.remove(&tab_id);
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.modal = Modal::None;
        self.last_sizes.clear();
        cx.notify();
    }

    fn begin_group_rename(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        if let Some((label, _)) = self.group_metadata(tab_id) {
            self.modal = Modal::GroupRename(GroupRenameEditor {
                tab_id,
                value: label,
                replace_on_type: true,
            });
            cx.notify();
        }
    }

    fn begin_rename(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focus_pane_with_snapshot(pane_id);
        if let Some(pane) = self.pane_metadata(pane_id) {
            self.modal = Modal::PaneRename(RenameEditor {
                pane_id,
                value: pane.title,
                replace_on_type: true,
            });
            cx.notify();
        }
    }

    fn toggle_tab_identity_picker(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        if let Modal::TabMenu(menu) = &mut self.modal
            && menu.pane_id == pane_id
        {
            menu.identity_picker_open = !menu.identity_picker_open;
            self.color_picker = None;
            cx.notify();
        }
    }
    fn toggle_group_icon_picker(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        if let Modal::GroupMenu(menu) = &mut self.modal
            && menu.tab_id == tab_id
        {
            menu.icon_picker_open = !menu.icon_picker_open;
            self.color_picker = None;
            cx.notify();
        }
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Modal::PaneRename(editor) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.send(&ClientRequest::RenamePane {
            pane_id: editor.pane_id,
            title: editor.value,
        });
        cx.notify();
    }

    fn submit_group_rename(&mut self, cx: &mut Context<Self>) {
        let Modal::GroupRename(editor) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.send(&ClientRequest::RenameTab {
            tab_id: editor.tab_id,
            title: editor.value,
        });
        cx.notify();
    }

    fn set_pane_profile(
        &mut self,
        pane_id: Uuid,
        profile: Option<TerminalProfile>,
        cx: &mut Context<Self>,
    ) {
        self.send(&ClientRequest::SetPaneProfile { pane_id, profile });
        self.modal = Modal::None;
        cx.notify();
    }

    fn set_custom_icon(
        &mut self,
        target: CustomIconTarget,
        icon: Option<String>,
        cx: &mut Context<Self>,
    ) {
        match target {
            CustomIconTarget::Pane(pane_id) => {
                self.send(&ClientRequest::SetPaneCustomIcon { pane_id, icon });
            }
            CustomIconTarget::Tab(tab_id) => {
                self.send(&ClientRequest::SetTabCustomIcon { tab_id, icon });
            }
        }
        self.modal = Modal::None;
        cx.notify();
    }

    fn set_pane_custom_icon(
        &mut self,
        pane_id: Uuid,
        icon: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.set_custom_icon(CustomIconTarget::Pane(pane_id), icon, cx);
    }

    fn set_tab_custom_icon(&mut self, tab_id: Uuid, icon: Option<String>, cx: &mut Context<Self>) {
        self.set_custom_icon(CustomIconTarget::Tab(tab_id), icon, cx);
    }

    fn import_custom_icon_for(&mut self, target: CustomIconTarget, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose tab image".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match selection.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("custom icon picker failed: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let result = cx
                .background_spawn(async move { import_custom_icon(&path) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(icon) => {
                    let icon_id = icon.id.clone();
                    if !this.custom_icons.iter().any(|saved| saved.id == icon_id) {
                        this.custom_icons.push(icon);
                    }
                    this.set_custom_icon(target, Some(icon_id), cx);
                }
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn import_pane_custom_icon(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.import_custom_icon_for(CustomIconTarget::Pane(pane_id), cx);
    }

    fn import_tab_custom_icon(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        self.import_custom_icon_for(CustomIconTarget::Tab(tab_id), cx);
    }
    fn detect_and_set_project_icon(
        &mut self,
        tab_id: Uuid,
        project_dir: String,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    detected_project_icon(Path::new(&project_dir))
                        .map(|path| import_custom_icon(&path))
                        .transpose()
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Some(icon)) => {
                    let icon_id = icon.id.clone();
                    if !this.custom_icons.iter().any(|saved| saved.id == icon_id) {
                        this.custom_icons.push(icon);
                    }
                    this.send(&ClientRequest::SetTabCustomIcon {
                        tab_id,
                        icon: Some(icon_id),
                    });
                    cx.notify();
                }
                Ok(None) => {}
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn choose_workstation_banner(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            self.report(&anyhow::anyhow!(
                "application state is unavailable; cannot save a custom banner"
            ));
            cx.notify();
            return;
        };
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose workstation banner".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match selection.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("workstation banner picker failed: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let result = cx
                .background_spawn(async move { store.import_workstation_banner(&path) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(stored) => {
                    this.workstation_banner = Some(BannerArtwork {
                        image: Arc::new(Image::from_bytes(ImageFormat::Png, stored.png)),
                        width: stored.width,
                        height: stored.height,
                    });
                    cx.notify();
                }
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn prompt_local_directory(
        &mut self,
        prompt: &'static str,
        on_pick: impl Fn(&mut Self, String, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match selection.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("directory picker failed: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let dir = path.to_string_lossy().into_owned();
            if let Err(message) = validate_workspace_dir(&dir) {
                let error = anyhow::anyhow!(message);
                let _ = this.update(cx, |this, cx| {
                    this.report(&error);
                    cx.notify();
                });
                return;
            }
            let _ = this.update(cx, move |this, cx| on_pick(this, dir, cx));
        })
        .detach();
    }

    fn reset_workstation_banner(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            self.report(&anyhow::anyhow!(
                "application state is unavailable; cannot reset the custom banner"
            ));
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { store.reset_workstation_banner() })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.workstation_banner = None;
                    cx.notify();
                }
                Err(error) => {
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_workstation_banner_visibility(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            self.report(&anyhow::anyhow!(
                "application state is unavailable; cannot save the banner visibility"
            ));
            cx.notify();
            return;
        };
        let hidden = !self.workstation_banner_hidden;
        self.workstation_banner_hidden = hidden;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { store.save_workstation_banner_hidden(hidden) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.workstation_banner_hidden = !hidden;
                    this.report(&error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn reset_pane_identity(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.send(&ClientRequest::ResetPaneIdentity { pane_id });
        self.modal = Modal::None;
        cx.notify();
    }

    fn begin_close(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        self.focus_pane_with_snapshot(pane_id);
        if let Some(pane) = self.pane_metadata(pane_id) {
            let leaves_workspace_empty = self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.workspaces.iter().any(|workspace| {
                    let panes = workspace
                        .tabs
                        .iter()
                        .flat_map(|tab| visible_panes(&tab.layout))
                        .collect::<Vec<_>>();
                    panes.len() == 1 && panes[0] == pane_id
                })
            });
            self.modal = Modal::Close(CloseConfirmation::for_pane(&pane, leaves_workspace_empty));
            cx.notify();
        }
    }

    fn begin_tab_close(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let confirmation = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot.workspaces.iter().find_map(|workspace| {
                let tab = workspace.tabs.iter().find(|tab| tab.id == tab_id)?;
                let child_count = workspace
                    .tabs
                    .iter()
                    .filter(|candidate| candidate.parent_tab == Some(tab_id))
                    .count();
                let mut panes = Vec::new();
                for candidate in workspace.tabs.iter().filter(|candidate| {
                    candidate.id == tab_id || candidate.parent_tab == Some(tab_id)
                }) {
                    collect_terminal_tabs(&candidate.layout, &mut panes);
                }
                Some(TabCloseConfirmation {
                    tab_id,
                    title: tab
                        .custom_title
                        .clone()
                        .unwrap_or_else(|| tab.title.clone()),
                    is_project: tab.project_dir.is_some(),
                    child_count,
                    terminal_count: panes.len(),
                })
            })
        });
        if let Some(confirmation) = confirmation {
            self.modal = Modal::TabClose(confirmation);
            cx.notify();
        }
    }

    fn confirm_tab_close(&mut self, cx: &mut Context<Self>) {
        let Modal::TabClose(confirmation) = std::mem::take(&mut self.modal) else {
            return;
        };
        match self.call(&ClientRequest::CloseTab {
            tab_id: confirmation.tab_id,
        }) {
            Ok(ServiceResponse::Ack) => {
                self.refresh_state();
                self.last_sizes.clear();
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn confirm_close(&mut self, cx: &mut Context<Self>) {
        let Modal::Close(confirmation) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.send(&confirmation.request());
        self.last_sizes.clear();
        cx.notify();
    }

    fn begin_workspace_creation(&mut self, cx: &mut Context<Self>) {
        self.modal = Modal::WorkspaceCreation(WorkspaceCreationDialog::new());
        self.workspace_input_layouts = [None, None];
        self.workspace_input_bounds = [None, None];
        cx.notify();
    }

    fn focus_workspace_creation_field(
        &mut self,
        field: WorkspaceCreationField,
        position: Option<Point<Pixels>>,
        extend_selection: bool,
        click_count: usize,
        window: &mut Window,
    ) {
        let index = field.index();
        let offset = position.and_then(|position| {
            let line = self.workspace_input_layouts[index].as_ref()?;
            let bounds = self.workspace_input_bounds[index]?;
            Some(line.closest_index_for_x(position.x - bounds.left()))
        });
        let Some(dialog) = self.modal.workspace_creation_mut() else {
            return;
        };
        dialog.field = field;
        // Give the custom GPUI input the platform text focus immediately on
        // mouse-down. Waiting for the next render left click/keyboard routing
        // competing with the terminal behind the modal on macOS.
        self.workspace_input_focus[index].focus(window);
        let editor = dialog.active_editor_mut();
        match offset {
            Some(_) if click_count >= 3 => editor.select_all(),
            Some(offset) if click_count == 2 => editor.select_word_at(offset),
            Some(offset) if extend_selection => editor.select_to(offset),
            Some(offset) => editor.move_to(offset),
            None => editor.move_end(false),
        }
    }

    fn submit_workspace_creation(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.modal.workspace_creation_mut() else {
            return;
        };
        if dialog.kind == WorkspaceCreationKind::SystemSsh
            && dialog.step == WorkspaceCreationStep::Details
        {
            dialog.review();
            cx.notify();
            return;
        }
        let Some(request_message) = self
            .modal
            .workspace_creation()
            .and_then(WorkspaceCreationDialog::approved_request)
        else {
            return;
        };
        match self.call(&request_message) {
            Ok(ServiceResponse::WorkspaceCreated {
                workspace_id,
                pane_id,
            }) => {
                self.active_workspace = Some(workspace_id);
                self.expanded_workspaces.insert(workspace_id);
                self.focus_pane_with_snapshot(pane_id);
                self.modal = Modal::None;
                self.refresh_state();
            }
            Ok(response) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    dialog.error = Some(format!("unexpected response: {response:?}"));
                }
            }
            Err(error) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    dialog.error = Some(format!("{error:#}"));
                }
            }
        }
        cx.notify();
    }

    fn begin_workspace_rename(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        if let Some(workspace) = workspace {
            self.modal = Modal::WorkspaceRename(WorkspaceRenameEditor {
                workspace_id,
                value: workspace.title.clone(),
                replace_on_type: true,
            });
            cx.notify();
        }
    }

    fn submit_workspace_rename(&mut self, cx: &mut Context<Self>) {
        let Modal::WorkspaceRename(editor) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.send(&ClientRequest::RenameWorkspace {
            workspace_id: editor.workspace_id,
            title: editor.value,
        });
        cx.notify();
    }

    fn begin_workspace_dir_edit(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| (workspace.connection.clone(), workspace.working_dir.clone()))
        });
        let Some((connection, working_dir)) = workspace else {
            self.modal = Modal::None;
            cx.notify();
            return;
        };
        match connection {
            WorkspaceConnection::Local => {
                self.modal = Modal::None;
                self.prompt_local_directory(
                    "Choose working directory",
                    move |this, dir, cx| {
                        this.send(&ClientRequest::SetWorkspaceWorkingDir {
                            workspace_id,
                            working_dir: Some(dir),
                        });
                        this.refresh_state();
                        cx.notify();
                    },
                    cx,
                );
            }
            WorkspaceConnection::SystemSsh { .. } => {
                self.modal = Modal::DirEditor(DirEditor {
                    target: DirEditorTarget::WorkspaceDefault(workspace_id),
                    value: working_dir.unwrap_or_else(|| "/".to_owned()),
                    replace_on_type: true,
                    suggestions: Vec::new(),
                });
                cx.notify();
            }
        }
    }

    fn begin_project_creation(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| (workspace.connection.clone(), workspace.working_dir.clone()))
        });
        let Some((connection, working_dir)) = workspace else {
            self.modal = Modal::None;
            cx.notify();
            return;
        };
        match connection {
            WorkspaceConnection::Local => {
                self.modal = Modal::None;
                self.prompt_local_directory(
                    "Choose project folder",
                    move |this, dir, cx| {
                        let project_dir = dir.clone();
                        match this.call(&ClientRequest::CreateWorkspaceProject {
                            workspace_id,
                            working_dir: dir,
                            title: None,
                        }) {
                            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                                this.focus_created_pane(workspace_id, pane_id, cx);
                                let tab_id = this.snapshot.as_ref().and_then(|snapshot| {
                                    snapshot
                                        .workspaces
                                        .iter()
                                        .find(|workspace| workspace.id == workspace_id)
                                        .and_then(|workspace| {
                                            workspace
                                                .tabs
                                                .iter()
                                                .find(|tab| {
                                                    find_pane(&tab.layout, pane_id).is_some()
                                                })
                                                .map(|tab| tab.id)
                                        })
                                });
                                if let Some(tab_id) = tab_id {
                                    this.detect_and_set_project_icon(tab_id, project_dir, cx);
                                }
                            }
                            Ok(response) => this.report_unexpected(&response),
                            Err(error) => this.report(&error),
                        }
                        this.refresh_state();
                        this.last_sizes.clear();
                        cx.notify();
                    },
                    cx,
                );
            }
            WorkspaceConnection::SystemSsh { .. } => {
                self.modal = Modal::DirEditor(DirEditor {
                    target: DirEditorTarget::NewProject(workspace_id),
                    value: working_dir.unwrap_or_else(|| "/".to_owned()),
                    replace_on_type: true,
                    suggestions: Vec::new(),
                });
                cx.notify();
            }
        }
    }

    fn begin_project_dir_edit(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let project = self.workspace_id_for_tab(tab_id).and_then(|workspace_id| {
            self.snapshot.as_ref().and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == workspace_id)
                    .and_then(|workspace| {
                        workspace
                            .tabs
                            .iter()
                            .find(|tab| tab.id == tab_id)
                            .map(|tab| {
                                (
                                    workspace.connection.clone(),
                                    tab.project_dir.clone().unwrap_or_else(|| "/".to_owned()),
                                )
                            })
                    })
            })
        });
        let Some((connection, value)) = project else {
            self.modal = Modal::None;
            cx.notify();
            return;
        };
        match connection {
            WorkspaceConnection::Local => {
                self.modal = Modal::None;
                self.prompt_local_directory(
                    "Choose project directory",
                    move |this, dir, cx| {
                        this.send(&ClientRequest::SetTabWorkingDir {
                            tab_id,
                            working_dir: dir,
                        });
                        this.refresh_state();
                        cx.notify();
                    },
                    cx,
                );
            }
            WorkspaceConnection::SystemSsh { .. } => {
                self.modal = Modal::DirEditor(DirEditor {
                    target: DirEditorTarget::ProjectDir(tab_id),
                    value,
                    replace_on_type: true,
                    suggestions: Vec::new(),
                });
                cx.notify();
            }
        }
    }

    fn submit_dir_editor(&mut self, cx: &mut Context<Self>) {
        let Modal::DirEditor(editor) = std::mem::take(&mut self.modal) else {
            return;
        };
        if let Err(error) = validate_workspace_dir(&editor.value) {
            self.report(&anyhow::anyhow!(error));
            self.modal = Modal::DirEditor(DirEditor {
                replace_on_type: false,
                ..editor
            });
            cx.notify();
            return;
        }
        match editor.target {
            DirEditorTarget::WorkspaceDefault(workspace_id) => {
                self.send(&ClientRequest::SetWorkspaceWorkingDir {
                    workspace_id,
                    working_dir: Some(editor.value),
                });
            }
            DirEditorTarget::NewProject(workspace_id) => {
                match self.call(&ClientRequest::CreateWorkspaceProject {
                    workspace_id,
                    working_dir: editor.value,
                    title: None,
                }) {
                    Ok(ServiceResponse::PaneCreated { pane_id }) => {
                        self.focus_created_pane(workspace_id, pane_id, cx);
                    }
                    Ok(response) => self.report_unexpected(&response),
                    Err(error) => self.report(&error),
                }
            }
            DirEditorTarget::ProjectDir(tab_id) => {
                self.send(&ClientRequest::SetTabWorkingDir {
                    tab_id,
                    working_dir: editor.value,
                });
            }
        }
        self.refresh_state();
        self.last_sizes.clear();
        cx.notify();
    }

    fn complete_remote_directory(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let Some(value) = self.modal.dir_editor().map(|editor| editor.value.clone()) else {
            return;
        };
        let (directory, partial) = remote_completion_parts(&value);
        match self.call(&ClientRequest::ListRemoteDirectory {
            workspace_id,
            path: directory.clone(),
        }) {
            Ok(ServiceResponse::RemoteDirectory { entries, .. }) => {
                let matches = entries
                    .into_iter()
                    .filter(|entry| entry.starts_with(&partial))
                    .collect::<Vec<_>>();
                if let Some(editor) = self.modal.dir_editor_mut() {
                    match matches.as_slice() {
                        [] => {
                            editor.suggestions = vec!["(no matches)".to_owned()];
                        }
                        [only] => {
                            editor.value = format!("{directory}{only}/");
                            editor.replace_on_type = false;
                            editor.suggestions.clear();
                        }
                        _ => {
                            let prefix = longest_common_prefix(&matches);
                            editor.value = format!("{directory}{prefix}");
                            editor.replace_on_type = false;
                            editor.suggestions = matches.into_iter().take(8).collect();
                        }
                    }
                }
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn handle_dir_editor_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        if keystroke.key == "tab" {
            let workspace_id = self
                .modal
                .dir_editor()
                .and_then(|editor| match editor.target {
                    DirEditorTarget::WorkspaceDefault(workspace_id)
                    | DirEditorTarget::NewProject(workspace_id) => Some(workspace_id),
                    DirEditorTarget::ProjectDir(tab_id) => self.workspace_id_for_tab(tab_id),
                });
            let ssh_workspace = workspace_id.filter(|workspace_id| {
                self.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.workspaces.iter().any(|workspace| {
                        workspace.id == *workspace_id
                            && matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. })
                    })
                })
            });
            if let Some(workspace_id) = ssh_workspace {
                self.complete_remote_directory(workspace_id, cx);
            }
            return;
        }
        let Some(editor) = self.modal.dir_editor_mut() else {
            return;
        };
        if keystroke.modifiers.platform && keystroke.key.eq_ignore_ascii_case("a") {
            editor.replace_on_type = true;
        } else {
            match keystroke.key.as_str() {
                "enter" => {
                    self.submit_dir_editor(cx);
                    return;
                }
                "escape" => self.modal = Modal::None,
                "backspace" => {
                    if editor.replace_on_type {
                        editor.value.clear();
                    } else {
                        editor.value.pop();
                    }
                    editor.replace_on_type = false;
                    editor.suggestions.clear();
                }
                _ if !keystroke.modifiers.platform
                    && !keystroke.modifiers.control
                    && !keystroke.modifiers.alt =>
                {
                    if let Some(text) = &keystroke.key_char {
                        append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
                        editor.suggestions.clear();
                    }
                }
                _ => {}
            }
        }
        cx.notify();
    }

    fn set_workspace_pinned(&mut self, workspace_id: Uuid, pinned: bool, cx: &mut Context<Self>) {
        self.send(&ClientRequest::SetWorkspacePinned {
            workspace_id,
            pinned,
        });
        self.modal = Modal::None;
        cx.notify();
    }

    fn set_tab_pinned(&mut self, tab_id: Uuid, pinned: bool, cx: &mut Context<Self>) {
        self.send(&ClientRequest::SetTabPinned { tab_id, pinned });
        self.modal = Modal::None;
        cx.notify();
    }

    fn reorder_workspace(
        &mut self,
        workspace_id: Uuid,
        target_workspace_id: Uuid,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        self.send(&ClientRequest::ReorderWorkspace {
            workspace_id,
            target_workspace_id,
            after,
        });
        self.dragging_workspace = None;
        self.workspace_drop_preview = None;
        self.suppress_workspace_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    fn reorder_workspace_tab(
        &mut self,
        tab_id: Uuid,
        target_tab_id: Uuid,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        self.send(&ClientRequest::ReorderTab {
            tab_id,
            target_tab_id,
            after,
        });
        self.tab_drop_preview = None;
        self.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    fn move_tab_to_project(&mut self, tab_id: Uuid, project_tab: Uuid, cx: &mut Context<Self>) {
        self.send(&ClientRequest::MoveTabToProject {
            tab_id,
            project_tab,
        });
        self.tab_drop_preview = None;
        self.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    fn move_sidebar_pane_to_group(
        &mut self,
        source_pane: Uuid,
        target_tab: Uuid,
        cx: &mut Context<Self>,
    ) {
        match self.call(&ClientRequest::MovePaneToGroup {
            source_pane,
            target_tab,
        }) {
            Ok(ServiceResponse::Ack) => {
                self.focused_pane = Some(source_pane);
                self.refresh_state();
                self.last_sizes.clear();
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.tab_drop_preview = None;
        self.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    fn move_sidebar_pane_to_new_tab(
        &mut self,
        source_pane: Uuid,
        target_tab: Uuid,
        after: bool,
        parent_tab: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        match self.call(&ClientRequest::MovePaneToNewTab {
            source_pane,
            target_tab,
            after,
            parent_tab,
        }) {
            Ok(ServiceResponse::Ack) => {
                self.focused_pane = Some(source_pane);
                self.refresh_state();
                self.last_sizes.clear();
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        self.tab_drop_preview = None;
        self.suppress_tab_click_until =
            Some(Instant::now() + Duration::from_millis(DRAG_CLICK_SUPPRESSION_MS));
        cx.notify();
    }

    fn disconnect_workspace(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.send_control(&ClientRequest::DisconnectWorkspace { workspace_id });
        if self.connection_error.is_none() && self.active_workspace == Some(workspace_id) {
            self.active_workspace = None;
            self.focused_pane = None;
        }
        self.refresh_state();
        cx.notify();
    }

    fn open_workspace_connection_info(
        &mut self,
        workspace_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.modal = Modal::WorkspaceConnectionInfo(WorkspaceConnectionInfo {
            workspace_id,
            position,
        });
        cx.notify();
    }

    fn begin_workspace_disconnect(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        let Some(Workspace {
            title,
            connection:
                WorkspaceConnection::SystemSsh {
                    destination,
                    status: WorkspaceConnectionStatus::Connected,
                },
            ..
        }) = workspace
        else {
            return;
        };
        self.modal = Modal::WorkspaceDisconnect(WorkspaceDisconnectConfirmation {
            workspace_id,
            title: title.clone(),
            destination: destination.clone(),
        });
        cx.notify();
    }

    fn confirm_workspace_disconnect(&mut self, cx: &mut Context<Self>) {
        let Modal::WorkspaceDisconnect(confirmation) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.disconnect_workspace(confirmation.workspace_id, cx);
    }

    fn reconnect_workspace(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::ReconnectWorkspace { workspace_id }) {
            Ok(ServiceResponse::PaneCreated { pane_id }) => {
                self.active_workspace = Some(workspace_id);
                self.focus_pane_with_snapshot(pane_id);
                self.refresh_state();
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn begin_workspace_delete(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        if let Some(workspace) = workspace {
            self.modal = Modal::WorkspaceDelete(WorkspaceDeleteConfirmation {
                workspace_id,
                title: workspace.title.clone(),
                active_terminal_count: workspace.active_terminal_count,
            });
            cx.notify();
        }
    }

    fn scan_tmux_sessions(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.modal = Modal::None;
        match self.call(&ClientRequest::ScanTmuxSessions { workspace_id }) {
            Ok(ServiceResponse::TmuxSessions {
                scope,
                sessions,
                open_session_ids,
                no_server,
            }) => {
                self.modal = Modal::TmuxPicker(TmuxSessionPicker {
                    workspace_id,
                    scope,
                    sessions,
                    open_session_ids: open_session_ids.into_iter().collect(),
                    no_server,
                    selected_session_ids: HashSet::new(),
                    status: None,
                    error: None,
                });
                self.connection_error = None;
            }
            Ok(response) => {
                self.modal = Modal::TmuxPicker(TmuxSessionPicker {
                    workspace_id,
                    scope: TmuxScanScope::Local,
                    sessions: Vec::new(),
                    open_session_ids: HashSet::new(),
                    no_server: false,
                    selected_session_ids: HashSet::new(),
                    status: None,
                    error: Some(format!("unexpected scan response: {response:?}")),
                });
            }
            Err(error) => {
                self.modal = Modal::TmuxPicker(TmuxSessionPicker {
                    workspace_id,
                    scope: TmuxScanScope::Local,
                    sessions: Vec::new(),
                    open_session_ids: HashSet::new(),
                    no_server: false,
                    selected_session_ids: HashSet::new(),
                    status: None,
                    error: Some(error.to_string()),
                });
            }
        }
        cx.notify();
    }

    fn mutate_tmux_selection(&mut self, change: TmuxSelectionChange, cx: &mut Context<Self>) {
        if let Some(picker) = self.modal.tmux_picker_mut() {
            match change {
                TmuxSelectionChange::Session(session_id) => picker.toggle_session(&session_id),
                TmuxSelectionChange::All => picker.select_all_sessions(),
                TmuxSelectionChange::None => picker.clear_all_sessions(),
            }
            picker.status = None;
            picker.error = None;
            cx.notify();
        }
    }

    fn open_selected_tmux_sessions(&mut self, cx: &mut Context<Self>) {
        let Some((workspace_id, session_ids)) = self.modal.tmux_picker_mut().map(|picker| {
            (
                picker.workspace_id,
                picker.selected_session_ids_in_scan_order(),
            )
        }) else {
            return;
        };
        if session_ids.is_empty() {
            return;
        }
        match self.call(&ClientRequest::AttachTmuxSessions {
            workspace_id,
            session_ids,
        }) {
            Ok(ServiceResponse::TmuxSessionsAttached { pane_ids, skipped }) => {
                let opened = pane_ids.len();
                self.active_workspace = Some(workspace_id);
                if let Some(pane_id) = pane_ids.last().copied() {
                    self.focus_pane_with_snapshot(pane_id);
                }
                if skipped.is_empty() {
                    self.modal = Modal::None;
                } else if let Some(picker) = self.modal.tmux_picker_mut() {
                    picker.selected_session_ids = skipped
                        .iter()
                        .map(|issue| issue.session_id.clone())
                        .collect();
                    let detail = skipped
                        .iter()
                        .map(|issue| format!("{} ({})", issue.session_id, issue.message))
                        .collect::<Vec<_>>()
                        .join(", ");
                    picker.status = Some(if opened == 0 {
                        format!("No tmux tabs opened. Skipped: {detail}")
                    } else {
                        format!("Opened {opened} tmux tab(s). Skipped: {detail}")
                    });
                    picker.error = None;
                }
                self.refresh_state();
            }
            Ok(response) => {
                if let Some(picker) = self.modal.tmux_picker_mut() {
                    picker.error = Some(format!("unexpected tmux open response: {response:?}"));
                }
            }
            Err(error) => {
                if let Some(picker) = self.modal.tmux_picker_mut() {
                    picker.error = Some(error.to_string());
                }
            }
        }
        cx.notify();
    }

    fn confirm_workspace_delete(&mut self, cx: &mut Context<Self>) {
        let Modal::WorkspaceDelete(confirmation) = std::mem::take(&mut self.modal) else {
            return;
        };
        self.send_control(&ClientRequest::DeleteWorkspace {
            workspace_id: confirmation.workspace_id,
        });
        if self.connection_error.is_none()
            && self.active_workspace == Some(confirmation.workspace_id)
        {
            self.active_workspace = None;
            self.focused_pane = None;
        }
        self.refresh_state();
        cx.notify();
    }

    fn focus_direction(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return;
        };
        let panes = visible_panes(layout);
        let Some(current) = self.focused_pane else {
            return;
        };
        let Some(index) = panes.iter().position(|pane| *pane == current) else {
            return;
        };
        let next = if forward {
            (index + 1) % panes.len()
        } else if index == 0 {
            panes.len() - 1
        } else {
            index - 1
        };
        self.focus_pane_with_snapshot(panes[next]);
        if self.zoomed_pane.is_some() {
            self.zoomed_pane = self.focused_pane;
            self.last_sizes.clear();
            self.sync_pty_sizes(cx);
        }
        cx.notify();
    }

    fn execute_command(&mut self, command: AppCommand, cx: &mut Context<Self>) {
        if !matches!(self.modal, Modal::None | Modal::CommandPalette(_))
            && command != AppCommand::ShowCommandPalette
        {
            return;
        }
        if matches!(command, AppCommand::SplitRight | AppCommand::SplitDown)
            && self.focused_pane.is_some_and(|pane_id| {
                self.pane_metadata(pane_id)
                    .is_some_and(|pane| matches!(pane.kind, PaneKind::Browser { .. }))
            })
        {
            return;
        }
        self.modal = Modal::None;
        match command {
            AppCommand::NewWorkspace => self.new_workspace(cx),
            AppCommand::ToggleSidebar => self.toggle_sidebar(cx),
            AppCommand::NewTab => self.new_tab(cx),
            AppCommand::NewBrowserTab => self.new_browser_tab(cx),
            AppCommand::TerminalZoomIn => self.adjust_terminal_zoom(1, cx),
            AppCommand::TerminalZoomOut => self.adjust_terminal_zoom(-1, cx),
            AppCommand::SplitRight => self.split(SplitAxis::Horizontal, cx),
            AppCommand::SplitDown => self.split(SplitAxis::Vertical, cx),
            AppCommand::FocusLeft | AppCommand::FocusUp => self.focus_direction(false, cx),
            AppCommand::FocusRight | AppCommand::FocusDown => self.focus_direction(true, cx),
            AppCommand::ShowCommandPalette => {
                self.modal = Modal::CommandPalette(CommandPaletteState::default());
                cx.notify();
            }
            AppCommand::TogglePaneZoom => self.toggle_pane_zoom(cx),
            AppCommand::EqualizePanes => self.equalize_panes(cx),
            AppCommand::ReattachPane => {
                if let Some(pane_id) = self.focused_pane {
                    self.reattach_pane(pane_id, cx);
                }
            }
            AppCommand::ShowNotifications => self.toggle_sidebar_activity(cx),
        }
    }

    fn terminal_metrics(&self, pane_id: Uuid) -> TerminalCellMetrics {
        self.terminal_font.metrics_for_zoom_level(
            self.terminal_zoom_levels
                .get(&pane_id)
                .copied()
                .unwrap_or_default(),
        )
    }

    fn adjust_terminal_zoom(&mut self, delta: i8, cx: &mut Context<Self>) {
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        let current = self
            .terminal_zoom_levels
            .get(&pane_id)
            .copied()
            .unwrap_or_default();
        let next = adjusted_terminal_zoom_level(current, delta);
        if next == current {
            return;
        }
        if next == 0 {
            self.terminal_zoom_levels.remove(&pane_id);
        } else {
            self.terminal_zoom_levels.insert(pane_id, next);
        }
        self.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    fn toggle_pane_zoom(&mut self, cx: &mut Context<Self>) {
        let Some(focused) = self.focused_pane else {
            return;
        };
        self.zoomed_pane = if self.zoomed_pane == Some(focused) {
            None
        } else {
            Some(focused)
        };
        self.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    /// Respawns an exited terminal in place. For a tmux tab this re-attaches
    /// the same session; the PTY starts at the service's default size, so the
    /// pane geometry must be pushed again for tmux to redraw at full size.
    fn reattach_pane(&mut self, pane_id: Uuid, cx: &mut Context<Self>) {
        match self.call(&ClientRequest::ReattachPane { pane_id }) {
            Ok(ServiceResponse::Ack) => {
                if let Some(state) = self.pane_states.get_mut(&pane_id) {
                    state.exited = false;
                }
                self.focus_pane_with_snapshot(pane_id);
                self.last_sizes.clear();
                self.sync_pty_sizes(cx);
                self.refresh_state();
            }
            Ok(response) => self.report_unexpected(&response),
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn equalize_panes(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot).cloned() else {
            return;
        };
        if apply_layout_control_mutation(
            &layout,
            &mut self.split_ratios,
            LayoutControlMutation::Equalize,
        ) > 0
        {
            self.last_sizes.clear();
            self.sync_pty_sizes(cx);
            cx.notify();
        }
    }

    fn handle_palette_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mut execute = None;
        let mut close = false;
        if let Some(palette) = self.modal.command_palette_mut() {
            let matches = palette_matches(&palette.query, COMMAND_PALETTE_LIMIT)
                .into_iter()
                .filter(|item| {
                    item.command != AppCommand::NewBrowserTab || browser_command_available()
                })
                .collect::<Vec<_>>();
            let result_count = matches.len();
            match keystroke.key.as_str() {
                "escape" => close = true,
                "enter" => {
                    execute = matches.get(palette.selected).map(|item| item.command);
                }
                "up" => {
                    palette.selected = palette.selected.saturating_sub(1);
                    cx.notify();
                }
                "down" => {
                    palette.selected = (palette.selected + 1).min(result_count.saturating_sub(1));
                    cx.notify();
                }
                "backspace" => {
                    palette.query.pop();
                    palette.selected = 0;
                    cx.notify();
                }
                _ if !keystroke.modifiers.platform
                    && !keystroke.modifiers.control
                    && !keystroke.modifiers.alt =>
                {
                    if let Some(text) = &keystroke.key_char
                        && !text.chars().any(char::is_control)
                    {
                        palette.query.push_str(text);
                        palette.selected = 0;
                        cx.notify();
                    }
                }
                _ => {}
            }
        }
        if close {
            self.modal = Modal::None;
            cx.notify();
        } else if let Some(command) = execute {
            self.execute_command(command, cx);
        }
        // Palette keystrokes are modal and can never become PTY input.
        cx.stop_propagation();
    }

    fn copy_terminal(&mut self, _: &CopyTerminal, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self
            .browser_url_editor
            .as_ref()
            .filter(|editor| editor.replace_on_type)
        {
            cx.write_to_clipboard(ClipboardItem::new_string(editor.text.clone()));
            return;
        }
        if let Some(text) = self
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
            .and_then(|dialog| dialog.active_editor().selected_text())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
            return;
        }
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        match self.call(&ClientRequest::CopySelection { pane_id }) {
            Ok(ServiceResponse::SelectionText { text: Some(text) }) if !text.is_empty() => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.connection_error = None;
            }
            Ok(ServiceResponse::SelectionText { .. }) => {}
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn paste_terminal(&mut self, _: &PasteTerminal, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if route_workspace_creation_paste(self.modal.workspace_creation_mut(), &text) {
            cx.notify();
            return;
        }
        if self.browser_url_editor.is_some() {
            self.append_browser_url_text(&text);
            cx.notify();
            return;
        }
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        let bracketed = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::BRACKETED_PASTE));
        match prepare_paste(&text, bracketed) {
            Ok(bytes) => self.send_control(&ClientRequest::WriteInput { pane_id, bytes }),
            Err(message) => self.connection_error = Some(message.to_owned()),
        }
        cx.notify();
    }

    fn find_terminal(&mut self, _: &FindTerminal, _: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.modal, Modal::None | Modal::Search(_)) {
            return;
        }
        self.modal = Modal::Search(SearchEditor::default());
        self.ime_preedit.clear();
        cx.notify();
    }

    fn find_next_terminal(&mut self, _: &FindNextTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.run_search(true, cx);
    }

    fn run_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(pane_id) = self.focused_pane else {
            return;
        };
        let Some(editor) = self.modal.search() else {
            return;
        };
        if editor.query.is_empty() {
            if let Some(editor) = self.modal.search_mut() {
                editor.no_match = false;
            }
            cx.notify();
            return;
        }
        let query = editor.query.clone();
        match self.call(&ClientRequest::SearchPane {
            pane_id,
            query: query.clone(),
            forward,
        }) {
            Ok(ServiceResponse::SearchResult { found }) => {
                if !found {
                    let before = self
                        .archived_views
                        .get(&pane_id)
                        .map(|view| view.page.cursor);
                    match self.call(&ClientRequest::SearchArchivedHistory {
                        pane_id,
                        query: query.clone(),
                        before,
                    }) {
                        Ok(ServiceResponse::HistorySearchResult { page: Some(page) }) => {
                            let rows = self
                                .screens
                                .get(&pane_id)
                                .map_or(30, |screen| usize::from(screen.rows));
                            let first_line = page
                                .lines
                                .iter()
                                .position(|line| line.contains(&query))
                                .unwrap_or(0)
                                .min(page.lines.len().saturating_sub(rows));
                            self.archived_views.clear();
                            self.archived_views
                                .insert(pane_id, ArchivedView { page, first_line });
                            if let Some(editor) = self.modal.search_mut() {
                                editor.no_match = false;
                            }
                        }
                        Ok(ServiceResponse::HistorySearchResult { page: None }) => {
                            if let Some(editor) = self.modal.search_mut() {
                                editor.no_match = true;
                            }
                        }
                        Ok(response) => {
                            self.report_unexpected(&response);
                        }
                        Err(error) => self.report(&error),
                    }
                } else if let Some(editor) = self.modal.search_mut() {
                    editor.no_match = false;
                    self.archived_views.remove(&pane_id);
                }
                self.connection_error = None;
                self.refresh_state();
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() || text.chars().any(|character| character == '\0') {
            return;
        }
        if let Some(picker) = self.color_picker.as_mut() {
            if picker.replace_on_type {
                picker.hex.clear();
            }
            let remaining = 6_usize.saturating_sub(picker.hex.len());
            picker.hex.extend(
                text.chars()
                    .filter(char::is_ascii_hexdigit)
                    .map(|character| character.to_ascii_uppercase())
                    .take(remaining),
            );
            picker.replace_on_type = false;
            picker.invalid = false;
            cx.notify();
            return;
        }
        if let Some(editor) = self.history_editor.as_mut() {
            if editor.replace_on_type {
                editor.text.clear();
            }
            let remaining = 4_usize.saturating_sub(editor.text.len());
            editor
                .text
                .extend(text.chars().filter(char::is_ascii_digit).take(remaining));
            editor.replace_on_type = false;
            editor.invalid = false;
            cx.notify();
            return;
        }
        if self.append_browser_url_text(text) {
            cx.notify();
            return;
        }
        if let Some(dialog) = self.modal.workspace_creation_mut() {
            if dialog.step == WorkspaceCreationStep::Details {
                dialog.replace_text(None, text, false, None);
            }
            cx.notify();
            return;
        }
        if let Some(editor) = self.modal.workspace_rename_mut() {
            append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
            cx.notify();
            return;
        }
        if let Some(editor) = self.modal.pane_rename_mut() {
            append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
            cx.notify();
            return;
        }
        if let Some(editor) = self.modal.group_rename_mut() {
            append_rename_text(&mut editor.value, &mut editor.replace_on_type, text);
            cx.notify();
            return;
        }
        if let Some(editor) = self.modal.search_mut() {
            let remaining = 256_usize.saturating_sub(editor.query.chars().count());
            editor
                .query
                .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
            editor.no_match = false;
            self.run_search(true, cx);
            return;
        }
        if let Some(pane_id) = self.focused_pane {
            self.send_control(&ClientRequest::WriteInput {
                pane_id,
                bytes: text.as_bytes().to_vec(),
            });
            cx.notify();
        }
    }

    fn begin_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane_with_snapshot(pane_id);
        self.focus_handle.focus(window);
        let mouse_reporting = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING));
        if mouse_reporting && !event.modifiers.shift {
            if let Some(button) = terminal_mouse_button(event.button) {
                self.send_control(&ClientRequest::MouseInput {
                    pane_id,
                    point,
                    button,
                    action: TerminalMouseAction::Press,
                    modifiers: terminal_modifiers(event.modifiers),
                });
            }
        } else if event.button == MouseButton::Left {
            let kind = if event.modifiers.alt {
                TerminalSelectionKind::Block
            } else if event.click_count >= 3 {
                TerminalSelectionKind::Lines
            } else if event.click_count == 2 {
                TerminalSelectionKind::Semantic
            } else {
                TerminalSelectionKind::Simple
            };
            self.selection_drag = Some(SelectionDrag {
                pane_id,
                anchor: point,
                preserve_single_cell: matches!(
                    kind,
                    TerminalSelectionKind::Semantic | TerminalSelectionKind::Lines
                ),
            });
            self.send_control(&ClientRequest::BeginSelection {
                pane_id,
                point,
                kind,
            });
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn move_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if self
            .selection_drag
            .is_some_and(|selection| selection.pane_id == pane_id)
            && event.dragging()
        {
            self.send_control(&ClientRequest::UpdateSelection { pane_id, point });
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let mouse_motion = self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_MOTION));
        if mouse_motion && let Some(button) = event.pressed_button.and_then(terminal_mouse_button) {
            self.send_control(&ClientRequest::MouseInput {
                pane_id,
                point,
                button,
                action: TerminalMouseAction::Move,
                modifiers: terminal_modifiers(event.modifiers),
            });
            cx.stop_propagation();
        }
    }

    fn end_terminal_pointer(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &MouseUpEvent,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = self
            .selection_drag
            .take()
            .filter(|selection| selection.pane_id == pane_id)
        {
            if point == selection.anchor && !selection.preserve_single_cell {
                self.send_control(&ClientRequest::ClearSelection { pane_id });
            } else {
                self.send_control(&ClientRequest::UpdateSelection { pane_id, point });
            }
        } else if self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
            && let Some(button) = terminal_mouse_button(event.button)
        {
            self.send_control(&ClientRequest::MouseInput {
                pane_id,
                point,
                button,
                action: TerminalMouseAction::Release,
                modifiers: terminal_modifiers(event.modifiers),
            });
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn scroll_terminal(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        event: &ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let metrics = self.terminal_metrics(pane_id);
        let pixels = event.delta.pixel_delta(px(metrics.line_height));
        let lines = (f32::from(pixels.y) / metrics.line_height).round() as i32;
        let lines = if lines == 0 {
            if pixels.y < px(0.0) { -1 } else { 1 }
        } else {
            lines
        };
        if self.archived_views.contains_key(&pane_id) {
            self.scroll_archived_view(pane_id, lines, cx);
            cx.stop_propagation();
            return;
        }
        if self
            .screens
            .get(&pane_id)
            .is_some_and(|screen| screen.modes.contains(TerminalModes::MOUSE_REPORTING))
            && !event.modifiers.shift
        {
            self.send_control(&ClientRequest::MouseInput {
                pane_id,
                point,
                button: if lines > 0 {
                    TerminalMouseButton::WheelUp
                } else {
                    TerminalMouseButton::WheelDown
                },
                action: TerminalMouseAction::Press,
                modifiers: terminal_modifiers(event.modifiers),
            });
        } else if lines > 0
            && self.screens.get(&pane_id).is_some_and(|screen| {
                screen.display_offset >= screen.history_size && screen.history_size > 0
            })
        {
            self.load_archived_page(pane_id, None, HistoryPageDirection::Older, cx);
        } else {
            self.send_control(&ClientRequest::ScrollPane { pane_id, lines });
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn load_archived_page(
        &mut self,
        pane_id: Uuid,
        cursor: Option<hh_protocol::HistoryCursor>,
        direction: HistoryPageDirection,
        cx: &mut Context<Self>,
    ) {
        match self.call(&ClientRequest::LoadHistoryPage {
            pane_id,
            cursor,
            direction,
        }) {
            Ok(ServiceResponse::HistoryPage { page: Some(page) }) => {
                let rows = self
                    .screens
                    .get(&pane_id)
                    .map_or(30, |screen| usize::from(screen.rows));
                let first_line = match direction {
                    HistoryPageDirection::Older => page.lines.len().saturating_sub(rows),
                    HistoryPageDirection::Newer => 0,
                };
                self.archived_views.clear();
                self.archived_views
                    .insert(pane_id, ArchivedView { page, first_line });
                self.connection_error = None;
            }
            Ok(ServiceResponse::HistoryPage { page: None }) => {
                if direction == HistoryPageDirection::Newer {
                    self.archived_views.remove(&pane_id);
                }
            }
            Ok(response) => {
                self.report_unexpected(&response);
            }
            Err(error) => self.report(&error),
        }
        cx.notify();
    }

    fn scroll_archived_view(&mut self, pane_id: Uuid, lines: i32, cx: &mut Context<Self>) {
        let rows = self
            .screens
            .get(&pane_id)
            .map_or(30, |screen| usize::from(screen.rows));
        let Some(view) = self.archived_views.get_mut(&pane_id) else {
            return;
        };
        if lines > 0 {
            let amount = usize::try_from(lines).unwrap_or(usize::MAX);
            if view.first_line > 0 {
                view.first_line = view.first_line.saturating_sub(amount);
                cx.notify();
                return;
            }
            if view.page.flags.contains(HistoryPageFlags::HAS_OLDER) {
                let cursor = view.page.cursor;
                self.load_archived_page(pane_id, Some(cursor), HistoryPageDirection::Older, cx);
            }
            return;
        }
        let amount = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        let maximum = view.page.lines.len().saturating_sub(rows);
        if view.first_line < maximum {
            view.first_line = view.first_line.saturating_add(amount).min(maximum);
            cx.notify();
            return;
        }
        if view.page.flags.contains(HistoryPageFlags::HAS_NEWER) {
            let cursor = view.page.cursor;
            self.load_archived_page(pane_id, Some(cursor), HistoryPageDirection::Newer, cx);
        } else {
            self.archived_views.remove(&pane_id);
            cx.notify();
        }
    }

    fn handle_workspace_creation_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        let step = self.modal.workspace_creation().map(|dialog| dialog.step);
        if step == Some(WorkspaceCreationStep::Details)
            && keystroke.modifiers.platform
            && keystroke.key.eq_ignore_ascii_case("a")
        {
            if let Some(dialog) = self.modal.workspace_creation_mut() {
                dialog.active_editor_mut().select_all();
                cx.notify();
            }
            return;
        }
        if step == Some(WorkspaceCreationStep::Details)
            && keystroke.modifiers.platform
            && keystroke.key.eq_ignore_ascii_case("x")
        {
            if let Some(dialog) = self.modal.workspace_creation_mut()
                && let Some(text) = dialog.active_editor().selected_text().map(str::to_owned)
            {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                dialog.replace_text(None, "", false, None);
                cx.notify();
            }
            return;
        }
        match keystroke.key.as_str() {
            "enter" => self.submit_workspace_creation(cx),
            "escape" => {
                self.modal = Modal::None;
                cx.notify();
            }
            "tab" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    dialog.field = match (dialog.kind, dialog.field) {
                        (WorkspaceCreationKind::SystemSsh, WorkspaceCreationField::Name) => {
                            WorkspaceCreationField::Destination
                        }
                        _ => WorkspaceCreationField::Name,
                    };
                    cx.notify();
                }
            }
            "backspace" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    dialog.backspace();
                    cx.notify();
                }
            }
            "delete" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    dialog.delete();
                    cx.notify();
                }
            }
            "left" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    if keystroke.modifiers.platform {
                        dialog
                            .active_editor_mut()
                            .move_home(keystroke.modifiers.shift);
                    } else {
                        dialog
                            .active_editor_mut()
                            .move_left(keystroke.modifiers.shift);
                    }
                    cx.notify();
                }
            }
            "right" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    if keystroke.modifiers.platform {
                        dialog
                            .active_editor_mut()
                            .move_end(keystroke.modifiers.shift);
                    } else {
                        dialog
                            .active_editor_mut()
                            .move_right(keystroke.modifiers.shift);
                    }
                    cx.notify();
                }
            }
            "home" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    dialog
                        .active_editor_mut()
                        .move_home(keystroke.modifiers.shift);
                    cx.notify();
                }
            }
            "end" if step == Some(WorkspaceCreationStep::Details) => {
                if let Some(dialog) = self.modal.workspace_creation_mut() {
                    dialog
                        .active_editor_mut()
                        .move_end(keystroke.modifiers.shift);
                    cx.notify();
                }
            }
            _ if step == Some(WorkspaceCreationStep::Details)
                && !keystroke.modifiers.platform
                && !keystroke.modifiers.control
                && !keystroke.modifiers.alt =>
            {
                if let Some(text) = &keystroke.key_char
                    && !text.chars().any(char::is_control)
                    && let Some(dialog) = self.modal.workspace_creation_mut()
                {
                    dialog.replace_text(None, text, false, None);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn handle_rename_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        target: RenameTarget,
        cx: &mut Context<Self>,
    ) {
        let (value, replace_on_type) = match (&mut self.modal, target) {
            (Modal::WorkspaceRename(editor), RenameTarget::Workspace) => {
                (&mut editor.value, &mut editor.replace_on_type)
            }
            (Modal::PaneRename(editor), RenameTarget::Pane) => {
                (&mut editor.value, &mut editor.replace_on_type)
            }
            (Modal::GroupRename(editor), RenameTarget::Group) => {
                (&mut editor.value, &mut editor.replace_on_type)
            }
            _ => return,
        };
        if keystroke.modifiers.platform && keystroke.key.eq_ignore_ascii_case("a") {
            *replace_on_type = true;
            cx.notify();
            return;
        }
        match keystroke.key.as_str() {
            "enter" => match target {
                RenameTarget::Pane => self.submit_rename(cx),
                RenameTarget::Workspace => self.submit_workspace_rename(cx),
                RenameTarget::Group => self.submit_group_rename(cx),
            },
            "escape" => {
                self.modal = Modal::None;
                cx.notify();
            }
            "backspace" => {
                if *replace_on_type {
                    value.clear();
                } else {
                    value.pop();
                }
                *replace_on_type = false;
                cx.notify();
            }
            _ if !keystroke.modifiers.platform
                && !keystroke.modifiers.control
                && !keystroke.modifiers.alt =>
            {
                if let Some(text) = &keystroke.key_char {
                    append_rename_text(value, replace_on_type, text);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        match keystroke.key.as_str() {
            "enter" => self.run_search(!keystroke.modifiers.shift, cx),
            "escape" => {
                self.modal = Modal::None;
                self.ime_preedit.clear();
                cx.notify();
            }
            "backspace" => {
                let Some(editor) = self.modal.search_mut() else {
                    return;
                };
                editor.query.pop();
                editor.no_match = false;
                let empty = editor.query.is_empty();
                if empty {
                    if let Some(pane_id) = self.focused_pane {
                        self.send_control(&ClientRequest::ClearSelection { pane_id });
                    }
                } else {
                    self.run_search(true, cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.sidebar_resize.is_active() {
            self.cancel_sidebar_resize(window, cx);
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.sidebar_activity {
            self.sidebar_activity = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.browser_url_editor.is_some() {
            let route_to_input_context = match event.keystroke.key.as_str() {
                "enter" => {
                    self.submit_browser_url(cx);
                    false
                }
                "escape" => {
                    self.browser_url_editor = None;
                    self.ime_preedit.clear();
                    cx.notify();
                    false
                }
                "backspace" => {
                    if let Some(editor) = self.browser_url_editor.as_mut() {
                        if editor.replace_on_type {
                            editor.text.clear();
                        } else {
                            editor.text.pop();
                        }
                        editor.replace_on_type = false;
                        editor.invalid = false;
                    }
                    cx.notify();
                    false
                }
                "a" if event.keystroke.modifiers.platform => {
                    if let Some(editor) = self.browser_url_editor.as_mut() {
                        editor.replace_on_type = true;
                    }
                    cx.notify();
                    false
                }
                _ if event.keystroke.modifiers.platform => true,
                _ if !event.keystroke.modifiers.platform && !event.keystroke.modifiers.control => {
                    let text = browser_key_text(
                        &event.keystroke.key,
                        event.keystroke.key_char.as_deref(),
                        event.keystroke.modifiers.shift,
                        event.keystroke.modifiers.alt,
                    );
                    if let Some(text) = text {
                        self.append_browser_url_text(&text);
                        cx.notify();
                        false
                    } else {
                        // IME/composition keys still need AppKit's input context.
                        true
                    }
                }
                _ => false,
            };
            if !route_to_input_context {
                cx.stop_propagation();
            }
            return;
        }
        if self.modal.command_palette().is_some() {
            self.handle_palette_key(event, cx);
            return;
        }
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform && !keystroke.modifiers.control && !keystroke.modifiers.alt
        {
            let workspace_shortcut = match keystroke.key.as_str() {
                "1" => Some(0),
                "2" => Some(1),
                "3" => Some(2),
                "4" => Some(3),
                "5" => Some(4),
                "6" => Some(5),
                "7" => Some(6),
                "8" => Some(7),
                "9" => Some(8),
                _ => None,
            };
            if workspace_shortcut.is_some_and(|index| self.select_workspace_by_index(index, cx)) {
                cx.stop_propagation();
                return;
            }
        }
        if let Some(picker) = self.color_picker.as_mut() {
            match keystroke.key.as_str() {
                "enter" => self.submit_color_picker(cx),
                "escape" => {
                    self.color_picker = None;
                    cx.notify();
                }
                "backspace" => {
                    if picker.replace_on_type {
                        picker.hex.clear();
                    } else {
                        picker.hex.pop();
                    }
                    picker.replace_on_type = false;
                    picker.invalid = false;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if let Some(editor) = self.history_editor.as_mut() {
            match keystroke.key.as_str() {
                "enter" => self.submit_history_edit(cx),
                "escape" => {
                    self.history_editor = None;
                    cx.notify();
                }
                "backspace" => {
                    if editor.replace_on_type {
                        editor.text.clear();
                    } else {
                        editor.text.pop();
                    }
                    editor.replace_on_type = false;
                    editor.invalid = false;
                    cx.notify();
                }
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        match &self.modal {
            Modal::None => {}
            Modal::CommandPalette(_) => {
                self.handle_palette_key(event, cx);
                return;
            }
            Modal::AppearanceSettings => {
                if keystroke.key == "escape" {
                    self.modal = Modal::None;
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceCreation(_) => {
                self.handle_workspace_creation_key(keystroke, cx);
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceRename(_) => {
                self.handle_rename_key(keystroke, RenameTarget::Workspace, cx);
                cx.stop_propagation();
                return;
            }
            Modal::DirEditor(_) => {
                self.handle_dir_editor_key(keystroke, cx);
                cx.stop_propagation();
                return;
            }
            Modal::PaneRename(_) => {
                self.handle_rename_key(keystroke, RenameTarget::Pane, cx);
                cx.stop_propagation();
                return;
            }
            Modal::GroupRename(_) => {
                self.handle_rename_key(keystroke, RenameTarget::Group, cx);
                cx.stop_propagation();
                return;
            }
            Modal::Search(_) => {
                self.handle_search_key(keystroke, cx);
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceDelete(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_workspace_delete(cx),
                    "escape" => {
                        self.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::TabClose(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_tab_close(cx),
                    "escape" => {
                        self.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::TmuxPicker(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.open_selected_tmux_sessions(cx),
                    "escape" => {
                        self.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::WorkspaceDisconnect(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_workspace_disconnect(cx),
                    "escape" => {
                        self.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::Close(_) => {
                match keystroke.key.as_str() {
                    "enter" => self.confirm_close(cx),
                    "escape" => {
                        self.modal = Modal::None;
                        cx.notify();
                    }
                    _ => {}
                }
                cx.stop_propagation();
                return;
            }
            Modal::TabMenu(_)
            | Modal::WorkspaceMenu(_)
            | Modal::CreateMenu(_)
            | Modal::GroupMenu(_)
            | Modal::WorkspaceConnectionInfo(_) => {
                if keystroke.key == "escape" {
                    self.modal = Modal::None;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
            }
        }
        if self.dragging_pane.is_some() && keystroke.key == "escape" {
            self.dragging_pane = None;
            self.drag_hover.clear();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let bytes = terminal_input_bytes(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            keystroke.modifiers.control,
            keystroke.modifiers.alt,
            keystroke.modifiers.platform,
        );
        if let (Some(pane_id), Some(bytes)) = (self.focused_pane, bytes) {
            self.send_control(&ClientRequest::WriteInput { pane_id, bytes });
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn handle_resize(&mut self, event: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        match self.sidebar_resize.pointer_move(event.pressed_button) {
            SidebarResizeMove::Ignore => {}
            SidebarResizeMove::Update => {
                let window_width = f32::from(window.bounds().size.width);
                let next = constrained_sidebar_width(f32::from(event.position.x), window_width);
                if (self.preferred_sidebar_width - next).abs() > f32::EPSILON {
                    self.preferred_sidebar_width = next;
                    self.update_window_geometry(window);
                    self.last_sizes.clear();
                    self.sync_pty_sizes(cx);
                    cx.notify();
                }
                return;
            }
            SidebarResizeMove::Complete => {
                self.persist_sidebar_width(cx);
                cx.notify();
                return;
            }
        }
        let Some(drag) = self.resizing else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.resizing = None;
            cx.notify();
            return;
        }
        self.update_window_geometry(window);
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return;
        };
        let root = PixelRect {
            x: 0.0,
            y: 0.0,
            width: self.workspace_pixels.0,
            height: self.workspace_pixels.1,
        };
        let Some(split) = find_split_rect(layout, drag.split_id, root, &self.split_ratios) else {
            return;
        };
        let workspace_x = f32::from(event.position.x) - self.sidebar_pixels;
        let workspace_y = f32::from(event.position.y) - APP_CHROME_HEIGHT;
        let ratio = match drag.axis {
            SplitAxis::Horizontal => (workspace_x - split.x) / split.width.max(1.0),
            SplitAxis::Vertical => (workspace_y - split.y) / split.height.max(1.0),
        };
        self.split_ratios.insert(
            drag.split_id,
            effective_split_ratio(drag.axis, split.width, split.height, ratio),
        );
        self.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    fn update_window_geometry(&mut self, window: &Window) -> bool {
        let window_width = f32::from(window.bounds().size.width);
        let sidebar_pixels = sidebar_width_for_visibility(
            self.preferred_sidebar_width,
            window_width,
            self.sidebar_visible,
        );
        let next = workspace_pixel_size(
            window_width,
            f32::from(window.bounds().size.height),
            sidebar_pixels,
        );
        if self.workspace_pixels == next
            && (self.sidebar_pixels - sidebar_pixels).abs() < f32::EPSILON
        {
            return false;
        }
        self.sidebar_pixels = sidebar_pixels;
        self.workspace_pixels = next;
        true
    }

    fn persist_sidebar_width(&self, cx: &mut Context<Self>) {
        let Some(store) = self.ui_state_store.clone() else {
            return;
        };
        let width = self.preferred_sidebar_width;
        cx.background_spawn(async move {
            if let Err(error) = store.save_workspace_sidebar_width(width) {
                eprintln!("Harness Harlot sidebar width was not persisted: {error:#}");
            }
        })
        .detach();
    }

    fn cancel_sidebar_resize(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(initial_width) = self.sidebar_resize.cancel() else {
            return;
        };
        self.preferred_sidebar_width = initial_width;
        self.update_window_geometry(window);
        self.last_sizes.clear();
        self.sync_pty_sizes(cx);
        cx.notify();
    }

    fn finish_resize(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_resize.finish() {
            self.persist_sidebar_width(cx);
        }
        self.resizing = None;
        self.dragging_pane = None;
        self.drag_hover.clear();
        cx.notify();
    }

    fn sync_pty_sizes(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let Some(layout) = self.active_layout(snapshot) else {
            return;
        };
        let mut sizes = Vec::new();
        let projected = self
            .zoomed_pane
            .and_then(|pane_id| zoom_projection(layout, pane_id));
        collect_pane_sizes(
            projected.as_ref().unwrap_or(layout),
            self.workspace_pixels.0,
            self.workspace_pixels.1,
            &|pane_id| self.terminal_metrics(pane_id),
            &self.split_ratios,
            &mut sizes,
        );
        let changed = sizes.len() != self.last_sizes.len()
            || sizes.iter().any(|(pane_id, columns, rows)| {
                self.last_sizes.get(pane_id) != Some(&(*columns, *rows))
            });
        if !changed {
            return;
        }
        self.last_sizes.clear();
        self.last_sizes.extend(
            sizes
                .iter()
                .map(|(pane_id, columns, rows)| (*pane_id, (*columns, *rows))),
        );
        self.resize_generation = self.resize_generation.wrapping_add(1);
        let generation = self.resize_generation;
        let client = Arc::clone(&self.control_client);
        cx.spawn(async move |this, cx| {
            gpui::Timer::after(Duration::from_millis(PTY_RESIZE_DEBOUNCE_MS)).await;
            let Ok(true) = this.update(cx, |this, _| this.resize_generation == generation) else {
                return;
            };
            let result = cx
                .background_spawn(async move {
                    let mut first_error = None;
                    for (pane_id, columns, rows) in sizes {
                        let failure = match session_call(
                            &client,
                            &ClientRequest::ResizePane {
                                pane_id,
                                columns,
                                rows,
                            },
                        ) {
                            Ok(ServiceResponse::Ack) => None,
                            Ok(response) => Some(anyhow::anyhow!(
                                "unexpected resize response for {pane_id}: {response:?}"
                            )),
                            Err(error) => Some(error.context(format!("resize pane {pane_id}"))),
                        };
                        if first_error.is_none() {
                            first_error = failure;
                        }
                    }
                    if let Some(error) = first_error {
                        Err(error)
                    } else {
                        Ok(())
                    }
                })
                .await;
            let _ = this.update(cx, |this, _| {
                if let Err(error) = result
                    && this.resize_generation == generation
                {
                    this.last_sizes.clear();
                    this.report(&error);
                }
            });
        })
        .detach();
    }

    fn render_sidebar_activity(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut notifications = self.notifications.clone();
        notifications.sort_by_key(|notification| {
            (
                notification.read,
                !matches!(notification.kind, NotificationKind::Attention),
                std::cmp::Reverse(notification.at_ms),
            )
        });

        let rows = notifications.into_iter().map(|notification| {
            let notification_id = notification.id;
            let pane_id = notification.pane_id;
            let workspace_id = notification.workspace_id;
            let unread = !notification.read;
            let color = match notification.kind {
                NotificationKind::Completed => THEME.ansi[2],
                NotificationKind::Attention => THEME.danger,
                NotificationKind::Message => THEME.accent,
            };
            div()
                .id(("sidebar-activity-row", notification_id))
                .mx(px(8.0))
                .my(px(2.0))
                .px(px(7.0))
                .py(px(6.0))
                .rounded(px(5.0))
                .cursor_pointer()
                .hover(|element| element.bg(rgb(THEME.elevated)))
                .flex()
                .items_center()
                .gap(px(7.0))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_notification(notification_id, pane_id, workspace_id, cx);
                }))
                .child(render_terminal_profile_icon(
                    notification.profile,
                    THEME.muted,
                    18.0,
                ))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(rgb(if unread {
                                    THEME.foreground
                                } else {
                                    THEME.muted
                                }))
                                .child(notification.pane_title),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child(notification.workspace_title),
                        ),
                )
                .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(color)))
        });
        div()
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(34.0))
                    .px(px(8.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Activity"),
                    )
                    .child(
                        div()
                            .id("feed-mark-read")
                            .px(px(6.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.mark_all_notifications_read(cx)),
                            )
                            .child("Read all"),
                    )
                    .child(
                        div()
                            .id("feed-clear")
                            .px(px(6.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .on_click(cx.listener(|this, _, _, cx| this.clear_notifications(cx)))
                            .child("Clear"),
                    ),
            )
            .child(
                div()
                    .id("sidebar-activity")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut workspaces = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.workspaces.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        workspaces.sort_by_key(|workspace| (!workspace.pinned, workspace.order));
        let history_needs_attention = self
            .history_status
            .as_ref()
            .is_some_and(|status| status.warning.is_some());
        let unread_notifications = self.unread_notification_count();
        let unread_label = if unread_notifications > 99 {
            "99+".to_owned()
        } else {
            unread_notifications.to_string()
        };
        let banner = self
            .workstation_banner
            .clone()
            .unwrap_or_else(workstation_banner_artwork);
        let has_workspaces = !workspaces.is_empty();
        let sidebar_content_width = self.sidebar_pixels - SIDEBAR_RESIZE_HIT_WIDTH;
        let banner_aspect_ratio = banner.aspect_ratio();
        let banner_header_height =
            workstation_banner_header_height(sidebar_content_width, banner_aspect_ratio);
        let (banner_width, banner_height) = banner_fit_size(
            sidebar_content_width,
            banner_header_height,
            banner_aspect_ratio,
        );
        div()
            .w(px(sidebar_content_width))
            .h_full()
            .flex_none()
            .bg(rgb(THEME.sidebar))
            // The resize target remains a generous 12 px, while the visible
            // rail separation is intentionally a restrained hairline.
            .border_r(px(0.5))
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .when(!self.workstation_banner_hidden, |element| {
                element
                    .child(
                        div()
                            .id("workstation-banner")
                            .relative()
                            .w_full()
                            // The header follows the banner's own aspect ratio, clamped between
                            // WORKSTATION_BANNER_MIN_HEIGHT and
                            // WORKSTATION_BANNER_MAX_HEIGHT, so any uploaded shape shows whole.
                            // The image gets explicit pixel dimensions: percentage sizing here
                            // rendered a cropped image because gpui injects an aspect ratio
                            // during img layout.
                            .h(px(banner_header_height))
                            .flex_none()
                            .overflow_hidden()
                            .bg(rgb(THEME.terminal))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                img(banner.image)
                                    .id("workstation-banner-image")
                                    .w(px(banner_width))
                                    .h(px(banner_height))
                                    .object_fit(gpui::ObjectFit::Contain),
                            ),
                    )
                    .child(div().h(px(1.0)).flex_none().bg(rgb(THEME.border)))
            })
            .child(
                div()
                    .h(px(40.0))
                    .px(px(8.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("new-workspace")
                            .flex_none()
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.surface))
                            .border_1()
                            .border_color(rgb(THEME.border))
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.border_color(rgb(THEME.accent)))
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_click(cx.listener(
                                |this, event: &ClickEvent, _, cx| {
                                    this.modal = Modal::CreateMenu(CreateMenu {
                                        position: event.position(),
                                        target: CreateMenuTarget::Global,
                                    });
                                    cx.notify();
                                },
                            ))
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Create… (⌘N)".to_owned(),
                                })
                                .into()
                            })
                            .child("＋"),
                    )
                    .child(
                        div()
                            .id("notifications")
                            .relative()
                            .flex_none()
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .when(self.sidebar_activity, |element| {
                                element.bg(rgb(THEME.elevated))
                            })
                            .hover(|element| {
                                element
                                    .bg(rgb(THEME.elevated))
                                    .text_color(rgb(THEME.foreground))
                            })
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Notifications".to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar_activity(cx)))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(render_bell_icon(if self.sidebar_activity {
                                THEME.foreground
                            } else {
                                THEME.muted
                            }))
                            .when(unread_notifications > 0, |element| {
                                element.child(
                                    div()
                                        .absolute()
                                        .top(px(-3.0))
                                        .right(px(-5.0))
                                        .min_w(px(15.0))
                                        .h(px(14.0))
                                        .px(px(3.0))
                                        .rounded_full()
                                        .bg(rgb(THEME.danger))
                                        .font_family(".SystemUIFont")
                                        .text_size(px(9.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(rgb(0xffffff))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(unread_label),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("appearance-settings")
                            .relative()
                            .flex_none()
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .hover(|element| {
                                element
                                    .bg(rgb(THEME.elevated))
                                    .text_color(rgb(THEME.foreground))
                            })
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "Settings".to_owned(),
                                })
                                .into()
                            })
                            .on_click(
                                cx.listener(|this, _, _, cx| this.open_appearance_settings(cx)),
                            )
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("⚙")
                            .when(history_needs_attention, |element| {
                                element.child(
                                    div()
                                        .absolute()
                                        .top(px(3.0))
                                        .right(px(3.0))
                                        .w(px(5.0))
                                        .h(px(5.0))
                                        .rounded_full()
                                        .bg(rgb(THEME.danger)),
                                )
                            }),
                    ),
            )
            .child(div().h(px(1.0)).flex_none().bg(rgb(THEME.border)))
            .when(self.sidebar_activity, |element| {
                element.child(self.render_sidebar_activity(cx))
            })
            .when(!self.sidebar_activity, |element| {
                element.child(
                    div()
                        .id("sidebar-workstation-list")
                        .min_h(px(0.0))
                        .flex_1()
                        .overflow_y_scroll()
                        .children(
                workspaces
                    .into_iter()
                    .enumerate()
                    .map(|(index, workspace)| {
                        let pinned = workspace.pinned;
                        let active = Some(workspace.id) == self.active_workspace;
                        let workspace_id = workspace.id;
                        let offline = matches!(
                            &workspace.connection,
                            WorkspaceConnection::SystemSsh {
                                status: WorkspaceConnectionStatus::Offline,
                                ..
                            }
                        );
                        let connected = matches!(
                            &workspace.connection,
                            WorkspaceConnection::SystemSsh {
                                status: WorkspaceConnectionStatus::Connected,
                                ..
                            }
                        );
                        let workspace_title = workspace.title.clone();
                        let workspace_dir =
                            workspace.working_dir.as_deref().map(abbreviate_home);
                        let (pinned_entries, project_entries, floating_entries) =
                            partition_workstation_entries(workspace_tab_entries(workspace));
                        let flatten_entries =
                            |entries: Vec<WorkstationTabEntry<'_>>,
                             section: Option<SidebarSection>| {
                            entries
                                .into_iter()
                                .enumerate()
                                .flat_map(move |(entry_index, entry)| {
                                    let parent_id = entry.tab_id;
                                    let mut flattened = vec![(
                                        entry.tab_id,
                                        entry.group_label,
                                        entry.project_dir,
                                        entry.color,
                                        entry.custom_icon,
                                        None,
                                        entry.panes.into_iter().cloned().collect::<Vec<_>>(),
                                        section.map(|section| (section, entry_index == 0)),
                                    )];
                                    flattened.extend(entry.children.into_iter().map(|child| {
                                        (
                                            child.tab_id,
                                            child.group_label,
                                            child.project_dir,
                                            child.color,
                                            child.custom_icon,
                                            Some(parent_id),
                                            child.panes.into_iter().cloned().collect::<Vec<_>>(),
                                            section.map(|section| (section, false)),
                                        )
                                    }));
                                    flattened
                                })
                                .collect::<Vec<_>>()
                        };
                        let mut tab_entries =
                            flatten_entries(pinned_entries, Some(SidebarSection::Pinned));
                        tab_entries.extend(flatten_entries(
                            project_entries,
                            Some(SidebarSection::Projects),
                        ));
                        tab_entries.extend(flatten_entries(floating_entries, None));
                        let terminal_count = workspace_terminal_tabs(workspace).len();
                        let expanded = self.expanded_workspaces.contains(&workspace_id);
                        let workspace_color = self.workspace_color(workspace_id).as_rgb();
                        let card_color = workspace_color;
                        let active_text = readable_text_color(card_color);
                        let drop_preview = self.workspace_drop_preview;
                        let drop_above = drop_preview.is_some_and(|preview| {
                            preview.target_workspace_id == workspace_id && !preview.after
                        });
                        let drop_below = drop_preview.is_some_and(|preview| {
                            preview.target_workspace_id == workspace_id && preview.after
                        });
                        let drag = WorkspaceDrag {
                            workspace_id,
                            pinned,
                            title: workspace_title.clone(),
                            position: Point::default(),
                        };
                        div().child(
                                    div()
                                        .id(("workspace-section", element_key(workspace.id)))
                                        .mx(px(7.0))
                                        .mb(px(3.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.0))
                                        .child(
                                            div()
                                    .id(("workspace", element_key(workspace.id)))
                                    .h(px(if workspace_dir.is_some() { 42.0 } else { 31.0 }))
                                    .px(px(8.0))
                                    .rounded(px(6.0))
                                    .border_t(if drop_above { px(2.0) } else { px(0.0) })
                                    .border_b(if drop_below { px(2.0) } else { px(0.0) })
                                    .border_color(rgb(if drop_above || drop_below {
                                        THEME.accent
                                    } else {
                                        THEME.border
                                    }))
                                    .when(!offline || terminal_count == 0, |element| {
                                        element.cursor_pointer()
                                    })
                                    .when(offline, |element| element.bg(rgb(card_color)))
                                    .when(active || connected, |element| {
                                        element.bg(rgb(card_color))
                                    })
                                    .hover(|element| {
                                        if active || connected || offline {
                                            element
                                        } else {
                                            element.bg(rgb(THEME.surface))
                                        }
                                    })
                                    .when(!offline || terminal_count == 0, |element| {
                                        element.on_click(cx.listener(move |this, _, _, cx| {
                                            if click_suppression_active(
                                                &mut this.suppress_workspace_click_until,
                                                Instant::now(),
                                            ) {
                                                cx.notify();
                                                return;
                                            }
                                            this.select_workspace(workspace_id, cx);
                                        }))
                                    })
                                    .on_drag(drag, |info: &WorkspaceDrag, position, _, cx| {
                                        cx.new(|_| WorkspaceDrag {
                                            position,
                                            ..info.clone()
                                        })
                                    })
                                    .on_drag_move::<WorkspaceDrag>(cx.listener(
                                        move |this, event: &gpui::DragMoveEvent<WorkspaceDrag>, _, cx| {
                                            let drag = event.drag(cx);
                                            if drag.workspace_id != workspace_id
                                                && drag.pinned == pinned
                                                && event.bounds.contains(&event.event.position)
                                            {
                                                this.dragging_workspace = Some(drag.workspace_id);
                                                this.workspace_drop_preview = Some(WorkspaceDropPreview {
                                                    target_workspace_id: workspace_id,
                                                    after: event.event.position.y > event.bounds.center().y,
                                                });
                                                cx.stop_propagation();
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    .on_drop(cx.listener(move |this, info: &WorkspaceDrag, _, cx| {
                                        if info.workspace_id != workspace_id && info.pinned == pinned {
                                            let after = this.workspace_drop_preview
                                                .is_some_and(|preview| preview.target_workspace_id == workspace_id && preview.after);
                                            this.reorder_workspace(info.workspace_id, workspace_id, after, cx);
                                        }
                                        cx.stop_propagation();
                                    }))
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                            this.open_workspace_menu(
                                                workspace_id,
                                                event.position,
                                                cx,
                                            );
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .child(
                                        div()
                                            .id((
                                                "toggle-workspace-tabs",
                                                element_key(workspace_id),
                                            ))
                                            .flex_none()
                                            .w(px(14.0))
                                            .h(px(18.0))
                                            .cursor_pointer()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .font_family(".SystemUIFont")
                                            .text_sm()
                                            .text_color(if active || connected || offline {
                                                rgb(active_text)
                                            } else {
                                                rgb(THEME.muted)
                                            })
                                            .tooltip(move |_, cx| {
                                                cx.new(|_| TooltipView {
                                                    text: if expanded {
                                                        "Collapse workstation terminals".to_owned()
                                                    } else {
                                                        "Expand workstation terminals".to_owned()
                                                    },
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.toggle_workspace_expanded(workspace_id, cx);
                                                cx.stop_propagation();
                                            }))
                                            .child(if expanded { "⌄" } else { "›" }),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .overflow_hidden()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_sm()
                                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                                    .text_color(
                                                        if active || connected || offline {
                                                            rgb(active_text)
                                                        } else {
                                                            rgb(THEME.foreground)
                                                        },
                                                    )
                                                    .child(format!(
                                                        "{}  {workspace_title}",
                                                        index + 1
                                                    )),
                                            )
                                            .when_some(workspace_dir, |element, directory| {
                                                element.child(
                                                    div()
                                                        .min_w(px(0.0))
                                                        .whitespace_nowrap()
                                                        .font_family("SF Mono")
                                                        .text_size(px(9.0))
                                                        .text_color(rgb(THEME.dim))
                                                        .child(directory),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .id(("workspace-tab-count", element_key(workspace_id)))
                                            .flex_none()
                                            .min_w(px(18.0))
                                            .h(px(17.0))
                                            .px(px(5.0))
                                            .rounded_full()
                                            .bg(rgba(if active || connected || offline {
                                                0xffffff20
                                            } else {
                                                0xffffff0c
                                            }))
                                            .font_family("SF Mono")
                                            .text_size(px(9.5))
                                            .text_color(if active || connected || offline {
                                                rgb(active_text)
                                            } else {
                                                rgb(THEME.muted)
                                            })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .tooltip(move |_, cx| {
                                                cx.new(|_| TooltipView {
                                                    text: terminal_tab_count_label(terminal_count),
                                                })
                                                .into()
                                            })
                                            .child(terminal_count.to_string()),
                                    )
                                    .child(
                                        div()
                                            .id((
                                                "workspace-row-menu",
                                                element_key(workspace_id),
                                            ))
                                            .flex_none()
                                            .w(px(16.0))
                                            .h(px(18.0))
                                            .rounded(px(4.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .font_family(".SystemUIFont")
                                            .text_sm()
                                            .text_color(rgb(THEME.dim))
                                            .hover(|element| {
                                                element.text_color(rgb(THEME.foreground))
                                            })
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    move |this,
                                                          event: &MouseDownEvent,
                                                          _,
                                                          cx| {
                                                        this.open_workspace_menu(
                                                            workspace_id,
                                                            event.position,
                                                            cx,
                                                        );
                                                        cx.stop_propagation();
                                                    },
                                                ),
                                            )
                                            .child("…"),
                                    )
                                    .when(connected, |element| {
                                        element
                                            .child(
                                                div()
                                                    .id((
                                                        "workspace-connection-info",
                                                        element_key(workspace_id),
                                                    ))
                                                    .flex_none()
                                                    .w(px(16.0))
                                                    .h(px(16.0))
                                                    .rounded_full()
                                                    .cursor_pointer()
                                                    .font_family(".SystemUIFont")
                                                    .text_xs()
                                                    .text_color(rgb(active_text))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .hover(|element| element.bg(rgba(0xffffff20)))
                                                    .tooltip(|_, cx| {
                                                        cx.new(|_| TooltipView {
                                                            text: "Connection details".to_owned(),
                                                        })
                                                        .into()
                                                    })
                                                    .on_click(cx.listener(
                                                        move |this, event: &ClickEvent, _, cx| {
                                                            this.open_workspace_connection_info(
                                                                workspace_id,
                                                                event.position(),
                                                                cx,
                                                            );
                                                            cx.stop_propagation();
                                                        },
                                                    ))
                                                    .child("ⓘ"),
                                            )
                                            .child(
                                                div()
                                                    .id((
                                                        "workspace-connected-indicator",
                                                        element_key(workspace_id),
                                                    ))
                                                    .flex_none()
                                                    .w(px(8.0))
                                                    .h(px(8.0))
                                                    .rounded_full()
                                                    .bg(rgb(THEME.ansi[2]))
                                                    .tooltip(|_, cx| {
                                                        cx.new(|_| TooltipView {
                                                            text: "Connected".to_owned(),
                                                        })
                                                        .into()
                                                    }),
                                            )
                                    })
                                    .when(offline, |element| {
                                        element
                                            .child(
                                                div()
                                                    .id((
                                                        "reconnect-workspace",
                                                        element_key(workspace_id),
                                                    ))
                                                    .flex_none()
                                                    .w(px(18.0))
                                                    .h(px(18.0))
                                                    .rounded(px(4.0))
                                                    .cursor_pointer()
                                                    .font_family(".SystemUIFont")
                                                    .text_sm()
                                                    .text_color(rgb(THEME.ansi[2]))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .hover(|element| element.bg(rgba(0xffffff20)))
                                                    .tooltip(|_, cx| {
                                                        cx.new(|_| TooltipView {
                                                            text: "Reconnect with system OpenSSH"
                                                                .to_owned(),
                                                        })
                                                        .into()
                                                    })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.reconnect_workspace(workspace_id, cx);
                                                        cx.stop_propagation();
                                                    }))
                                                    .child("↻"),
                                            )
                                            .child(
                                                div()
                                                    .id((
                                                        "delete-offline-workspace",
                                                        element_key(workspace_id),
                                                    ))
                                                    .flex_none()
                                                    .w(px(18.0))
                                                    .h(px(18.0))
                                                    .rounded(px(4.0))
                                                    .cursor_pointer()
                                                    .font_family(".SystemUIFont")
                                                    .text_xs()
                                                    .text_color(rgb(THEME.danger))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .hover(|element| element.bg(rgba(0xffffff20)))
                                                    .tooltip(|_, cx| {
                                                        cx.new(|_| TooltipView {
                                                            text: "Delete saved workstation…"
                                                                .to_owned(),
                                                        })
                                                        .into()
                                                    })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.begin_workspace_delete(
                                                            workspace_id,
                                                            cx,
                                                        );
                                                        cx.stop_propagation();
                                                    }))
                                                    .child("⌫"),
                                            )
                                    }),
                            )
                            .when(expanded, |element| {
                                if terminal_count == 0 {
                                    element.child(
                                        div()
                                            .ml(px(28.0))
                                            .mr(px(4.0))
                                            .py(px(5.0))
                                            .font_family(".SystemUIFont")
                                            .text_xs()
                                            .text_color(rgb(THEME.dim))
                                            .child("No open terminal tabs"),
                                    )
                                } else {
                                    element.children(tab_entries.into_iter().flat_map(
                                        |(
                                            tab_id,
                                            group_label,
                                            project_dir,
                                            tab_color,
                                            custom_icon,
                                            parent_tab,
                                            panes,
                                            section,
                                        )| {
                                            let is_project = project_dir.is_some();
                                            let mut rows = Vec::new();
                                            if let Some((section, is_section_header)) = section {
                                                let collapsed = match section {
                                                    SidebarSection::Pinned => self
                                                        .collapsed_pinned_sections
                                                        .contains(&workspace_id),
                                                    SidebarSection::Projects => self
                                                        .collapsed_project_sections
                                                        .contains(&workspace_id),
                                                };
                                                if is_section_header {
                                                    rows.push(
                                                        div()
                                                            .id((
                                                                section.element_id(),
                                                                element_key(workspace_id),
                                                            ))
                                                            .ml(px(20.0))
                                                            .py(px(3.0))
                                                            .cursor_pointer()
                                                            .flex()
                                                            .items_center()
                                                            .gap(px(4.0))
                                                            .font_family(".SystemUIFont")
                                                            .text_xs()
                                                            .text_color(rgb(THEME.dim))
                                                            .hover(|element| {
                                                                element.text_color(rgb(
                                                                    THEME.muted,
                                                                ))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    this.toggle_sidebar_section(
                                                                        workspace_id, section, cx,
                                                                    );
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                            .child(
                                                                div().flex_none().child(
                                                                    if collapsed {
                                                                        "›"
                                                                    } else {
                                                                        "⌄"
                                                                    },
                                                                ),
                                                            )
                                                            .child(section.label())
                                                            .into_any_element(),
                                                    );
                                                }
                                                if collapsed {
                                                    return rows;
                                                }
                                            }
                                            if parent_tab.is_some_and(|parent_id| {
                                                self.collapsed_groups.contains(&parent_id)
                                            }) {
                                                return rows;
                                            }
                                            let group_indent =
                                                if parent_tab.is_some() { 48.0 } else { 20.0 };
                                            let pane_indent =
                                                if parent_tab.is_some() { 62.0 } else { 34.0 };
                                            let tab_active = self.focused_pane.is_some_and(
                                                |focused| panes.iter().any(|pane| pane.id == focused),
                                            );
                                            let tab_focus_target = self
                                                .focused_pane
                                                .filter(|focused| {
                                                    panes.iter().any(|pane| pane.id == *focused)
                                                })
                                                .or_else(|| panes.first().map(|pane| pane.id));
                                            match group_label {
                                                None => {
                                                    if let Some(pane) = panes.into_iter().next() {
                                                        rows.push(
                                                            self.render_workspace_terminal_row(
                                                                &pane,
                                                                SidebarPaneRowContext {
                                                                    workspace_id,
                                                                    tab_id: Some(tab_id),
                                                                    tab_color,
                                                                    from_group: false,
                                                                    indent: group_indent,
                                                                },
                                                                cx,
                                                            ),
                                                        );
                                                    }
                                                }
                                                Some(label) => {
                                                    let collapsed =
                                                        self.collapsed_groups.contains(&tab_id);
                                                    let count_label =
                                                        terminal_tab_count_label(panes.len());
                                                    let drop_preview = self.tab_drop_preview;
                                                    let drop_into =
                                                        drop_preview.is_some_and(|preview| {
                                                            preview.target_tab_id == tab_id
                                                                && preview.into_group
                                                        });
                                                    let drop_above =
                                                        drop_preview.is_some_and(|preview| {
                                                            preview.target_tab_id == tab_id
                                                                && !preview.into_group
                                                                && !preview.after
                                                        });
                                                    let drop_below =
                                                        drop_preview.is_some_and(|preview| {
                                                            preview.target_tab_id == tab_id
                                                                && !preview.into_group
                                                                && preview.after
                                                        });
                                                    let drag = TabDrag {
                                                        workspace_id,
                                                        tab_id,
                                                        pane_id: None,
                                                        from_group: false,
                                                        title: label.clone(),
                                                        position: Point::default(),
                                                    };
                                                    let custom_icon_path = custom_icon
                                                        .as_deref()
                                                        .and_then(|icon| self.custom_icon_path(icon));
                                                    rows.push(
                                                        div()
                                                            .id((
                                                                "workspace-group",
                                                                element_key(tab_id),
                                                            ))
                                                            .ml(px(group_indent))
                                                            .mr(px(4.0))
                                                            .px(px(7.0))
                                                            .h(px(27.0))
                                                            .rounded(px(4.0))
                                                            .border_t(if drop_above {
                                                                px(2.0)
                                                            } else {
                                                                px(0.0)
                                                            })
                                                            .border_b(if drop_below {
                                                                px(2.0)
                                                            } else {
                                                                px(0.0)
                                                            })
                                                            .when(drop_into, |element| {
                                                                element.border_1()
                                                            })
                                                            .border_color(rgb(if drop_into
                                                                || drop_above
                                                                || drop_below
                                                            {
                                                                THEME.accent
                                                            } else {
                                                                THEME.border
                                                            }))
                                                            .cursor_pointer()
                                                            .flex()
                                                            .items_center()
                                                            .gap(px(6.0))
                                                            .when_some(tab_color, |element, color| {
                                                                element.bg(rgba(rgba_with_alpha(
                                                                    color.as_rgb(),
                                                                    TAB_COLOR_ALPHA,
                                                                )))
                                                            })
                                                            .when(
                                                                tab_active && tab_color.is_none(),
                                                                |element| {
                                                                    element.bg(rgb(
                                                                        THEME.accent_soft,
                                                                    ))
                                                                },
                                                            )
                                                            .hover(|element| {
                                                                element.bg(rgb(THEME.elevated))
                                                            })
                                                            .on_click(cx.listener(
                                                                move |this, _, _, cx| {
                                                                    if click_suppression_active(
                                                                        &mut this
                                                                            .suppress_tab_click_until,
                                                                        Instant::now(),
                                                                    ) {
                                                                        cx.notify();
                                                                        return;
                                                                    }
                                                                    this.collapsed_groups
                                                                        .remove(&tab_id);
                                                                    if let Some(pane_id) =
                                                                        tab_focus_target
                                                                    {
                                                                        this.select_sidebar_pane(
                                                                            workspace_id,
                                                                            tab_id,
                                                                            pane_id,
                                                                            cx,
                                                                        );
                                                                    }
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                            .on_drag(
                                                                drag,
                                                                |info: &TabDrag,
                                                                 position,
                                                                 _,
                                                                 cx| {
                                                                    cx.new(|_| TabDrag {
                                                                        position,
                                                                        ..info.clone()
                                                                    })
                                                                },
                                                            )
                                                            .on_drag_move::<TabDrag>(cx.listener(
                                                                move |this,
                                                                      event:
                                                                          &gpui::DragMoveEvent<
                                                                            TabDrag,
                                                                        >,
                                                                      _,
                                                                      cx| {
                                                                    let drag = event.drag(cx);
                                                                    if drag.workspace_id
                                                                        != workspace_id
                                                                        || (drag.tab_id == tab_id
                                                                            && drag.pane_id.is_none())
                                                                    {
                                                                        if this
                                                                            .tab_drop_preview
                                                                            .take()
                                                                            .is_some()
                                                                        {
                                                                            cx.notify();
                                                                        }
                                                                        return;
                                                                    }
                                                                    if event.bounds.contains(
                                                                        &event.event.position,
                                                                    ) {
                                                                        let zone = header_drop_zone(
                                                                            f32::from(
                                                                                event
                                                                                    .event
                                                                                    .position
                                                                                    .y,
                                                                            ),
                                                                            f32::from(
                                                                                event.bounds.origin.y,
                                                                            ),
                                                                            f32::from(
                                                                                event.bounds.origin.y
                                                                                    + event
                                                                                        .bounds
                                                                                        .size
                                                                                        .height,
                                                                            ),
                                                                        );
                                                                        this.tab_drop_preview =
                                                                            Some(
                                                                                TabDropPreview {
                                                                                    target_tab_id:
                                                                                        tab_id,
                                                                                    after: zone
                                                                                        == HeaderDropZone::After,
                                                                                    into_group: zone
                                                                                        == HeaderDropZone::Into
                                                                                        && (is_project
                                                                                            || (drag
                                                                                                .pane_id
                                                                                                .is_some()
                                                                                                && drag
                                                                                                    .tab_id
                                                                                                    != tab_id)),
                                                                                },
                                                                            );
                                                                        cx.stop_propagation();
                                                                        cx.notify();
                                                                    }
                                                                },
                                                            ))
                                                            .on_drop(cx.listener(
                                                                move |this,
                                                                      info: &TabDrag,
                                                                      _,
                                                                      cx| {
                                                                    if info.workspace_id
                                                                        == workspace_id
                                                                    {
                                                                        let preview = this
                                                                            .tab_drop_preview
                                                                            .filter(|preview| {
                                                                                preview
                                                                                    .target_tab_id
                                                                                    == tab_id
                                                                            });
                                                                        let into_group = preview
                                                                            .is_some_and(
                                                                                |preview| {
                                                                                    preview
                                                                                        .into_group
                                                                                },
                                                                            );
                                                                        let after = preview
                                                                            .is_some_and(
                                                                                |preview| {
                                                                                    preview.after
                                                                                },
                                                                            );
                                                                        if into_group && is_project {
                                                                            if let Some(source_pane) =
                                                                                info.pane_id
                                                                                    .filter(|_| {
                                                                                        info.from_group
                                                                                    })
                                                                            {
                                                                                this.move_sidebar_pane_to_new_tab(
                                                                                    source_pane,
                                                                                    tab_id,
                                                                                    false,
                                                                                    Some(tab_id),
                                                                                    cx,
                                                                                );
                                                                            } else if info.tab_id
                                                                                != tab_id
                                                                            {
                                                                                this.move_tab_to_project(
                                                                                    info.tab_id,
                                                                                    tab_id,
                                                                                    cx,
                                                                                );
                                                                            }
                                                                        } else if into_group {
                                                                            if let Some(source_pane) =
                                                                                info.pane_id
                                                                            {
                                                                                this.move_sidebar_pane_to_group(
                                                                                    source_pane,
                                                                                    tab_id,
                                                                                    cx,
                                                                                );
                                                                            }
                                                                        } else if let Some(
                                                                            source_pane,
                                                                        ) = info.pane_id.filter(
                                                                            |_| info.from_group,
                                                                        ) {
                                                                            this.move_sidebar_pane_to_new_tab(
                                                                                source_pane,
                                                                                tab_id,
                                                                                after,
                                                                                None,
                                                                                cx,
                                                                            );
                                                                        } else if info.tab_id
                                                                            != tab_id
                                                                        {
                                                                            this.reorder_workspace_tab(
                                                                                info.tab_id,
                                                                                tab_id,
                                                                                after,
                                                                                cx,
                                                                            );
                                                                        }
                                                                    }
                                                                    cx.stop_propagation();
                                                                },
                                                            ))
                                                            .on_mouse_down(
                                                                MouseButton::Right,
                                                                cx.listener(
                                                                    move |this,
                                                                          event:
                                                                              &MouseDownEvent,
                                                                          _,
                                                                          cx| {
                                                                        this.open_group_menu(
                                                                            tab_id,
                                                                            event.position,
                                                                            cx,
                                                                        );
                                                                        cx.stop_propagation();
                                                                    },
                                                                ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .id((
                                                                        "toggle-workspace-group",
                                                                        element_key(tab_id),
                                                                    ))
                                                                    .flex_none()
                                                                    .w(px(12.0))
                                                                    .font_family(".SystemUIFont")
                                                                    .text_xs()
                                                                    .text_color(rgb(THEME.dim))
                                                                    .on_click(cx.listener(
                                                                        move |this, _, _, cx| {
                                                                            this.toggle_group_collapsed(
                                                                                tab_id, cx,
                                                                            );
                                                                            cx.stop_propagation();
                                                                        },
                                                                    ))
                                                                    .child(if collapsed {
                                                                        "▸"
                                                                    } else {
                                                                        "▾"
                                                                    }),
                                                            )
                                                            .when_some(custom_icon_path.clone(), |element, path| {
                                                                element.child(
                                                                    img(path)
                                                                        .flex_none()
                                                                        .w(px(11.0))
                                                                        .h(px(11.0))
                                                                        .object_fit(gpui::ObjectFit::Contain)
                                                                        .rounded(px(2.0)),
                                                                )
                                                            })
                                                            .when(
                                                                custom_icon_path.is_none()
                                                                    && project_dir.is_some(),
                                                                |element| {
                                                                    element.child(
                                                                        div()
                                                                            .relative()
                                                                            .flex_none()
                                                                            .w(px(11.0))
                                                                            .h(px(8.0))
                                                                            .child(
                                                                                div()
                                                                                    .absolute()
                                                                                    .left(px(0.0))
                                                                                    .top(px(2.0))
                                                                                    .w(px(11.0))
                                                                                    .h(px(6.0))
                                                                                    .rounded(px(1.5))
                                                                                    .border_1()
                                                                                    .border_color(rgb(THEME.muted)),
                                                                            )
                                                                            .child(
                                                                                div()
                                                                                    .absolute()
                                                                                    .left(px(0.0))
                                                                                    .top(px(0.0))
                                                                                    .w(px(5.0))
                                                                                    .h(px(3.0))
                                                                                    .rounded(px(1.0))
                                                                                    .bg(rgb(THEME.muted)),
                                                                            ),
                                                                    )
                                                                },
                                                            )
                                                            .child(
                                                                div()
                                                                    .min_w(px(0.0))
                                                                    .flex_1()
                                                                    .truncate()
                                                                    .font_family(".SystemUIFont")
                                                                    .text_xs()
                                                                    .text_color(rgb(
                                                                        THEME.foreground,
                                                                    ))
                                                                    .child(label),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex_none()
                                                                    .font_family(".SystemUIFont")
                                                                    .text_xs()
                                                                    .text_color(rgb(THEME.dim))
                                                                    .child(count_label),
                                                            )
                                                            .child(
                                                                div()
                                                                    .id((
                                                                        "group-row-menu",
                                                                        element_key(tab_id),
                                                                    ))
                                                                    .flex_none()
                                                                    .w(px(16.0))
                                                                    .h(px(18.0))
                                                                    .rounded(px(4.0))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .font_family(".SystemUIFont")
                                                                    .text_sm()
                                                                    .text_color(rgb(THEME.dim))
                                                                    .hover(|element| {
                                                                        element.text_color(rgb(
                                                                            THEME.foreground,
                                                                        ))
                                                                    })
                                                                    .on_mouse_down(
                                                                        MouseButton::Left,
                                                                        cx.listener(
                                                                            move |this,
                                                                                  event:
                                                                                      &MouseDownEvent,
                                                                                  _,
                                                                                  cx| {
                                                                                this.open_group_menu(
                                                                                    tab_id,
                                                                                    event.position,
                                                                                    cx,
                                                                                );
                                                                                cx.stop_propagation();
                                                                            },
                                                                        ),
                                                                    )
                                                                    .child("…"),
                                                            )
                                                            .into_any_element(),
                                                    );
                                                    if !collapsed {
                                                        rows.extend(panes.into_iter().map(|pane| {
                                                            self.render_workspace_terminal_row(
                                                                &pane,
                                                                SidebarPaneRowContext {
                                                                    workspace_id,
                                                                    tab_id: Some(tab_id),
                                                                    tab_color,
                                                                    from_group: true,
                                                                    indent: pane_indent,
                                                                },
                                                                cx,
                                                            )
                                                        }));
                                                    }
                                                }
                                            }
                                            rows
                                        },
                                    ))
                                }
                            })
                                )
                    }),
                )
                .when(!has_workspaces, |element| {
                    element.child(
                        div()
                            .px(px(14.0))
                            .pb(px(6.0))
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("No workstations yet — use Add workstation above"),
                    )
                }),
            )
            })
            .into_any_element()
    }

    fn custom_icon_path(&self, icon: &str) -> Option<PathBuf> {
        self.custom_icons
            .iter()
            .find(|saved| saved.id == icon)
            .map(|saved| saved.path.clone())
    }

    fn render_pane_identity_mark(
        &self,
        pane: &Pane,
        fallback_color: u32,
        frame_color: u32,
    ) -> AnyElement {
        if let Some(path) = pane
            .custom_icon
            .as_deref()
            .and_then(|icon| self.custom_icon_path(icon))
        {
            return img(path)
                .w(px(IDENTITY_MARK_SIZE))
                .h(px(IDENTITY_MARK_SIZE))
                .object_fit(gpui::ObjectFit::Contain)
                .rounded(px(3.0))
                .into_any_element();
        }
        if matches!(&pane.kind, PaneKind::Browser { .. }) {
            #[cfg(all(target_os = "macos", feature = "browser"))]
            if let Some(favicon) = self
                .browser_views
                .get(&pane.id)
                .and_then(|view| view.shared.borrow().favicon.clone())
            {
                return img(favicon)
                    .w(px(IDENTITY_MARK_SIZE))
                    .h(px(IDENTITY_MARK_SIZE))
                    .object_fit(gpui::ObjectFit::Contain)
                    .rounded(px(3.0))
                    .into_any_element();
            }
            return svg()
                .path("agent-icons/browser-globe.svg")
                .w(px(IDENTITY_MARK_SIZE))
                .h(px(IDENTITY_MARK_SIZE))
                .text_color(rgb(fallback_color))
                .into_any_element();
        }
        render_terminal_profile_mark(pane.identity.profile, fallback_color, frame_color)
    }

    fn render_workspace_terminal_row(
        &self,
        pane: &Pane,
        row: SidebarPaneRowContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SidebarPaneRowContext {
            workspace_id,
            tab_id,
            tab_color,
            from_group,
            indent,
        } = row;
        let pane_id = pane.id;
        let selected = self.focused_pane == Some(pane_id);
        let identity = tab_identity_presentation(pane);
        let identity_detail = identity.detail.clone();
        let drag_title = identity.label.clone();
        let drop_above = !from_group
            && tab_id.is_some_and(|tab_id| {
                self.tab_drop_preview.is_some_and(|preview| {
                    preview.target_tab_id == tab_id && !preview.into_group && !preview.after
                })
            });
        let drop_below = !from_group
            && tab_id.is_some_and(|tab_id| {
                self.tab_drop_preview.is_some_and(|preview| {
                    preview.target_tab_id == tab_id && !preview.into_group && preview.after
                })
            });
        let pane_accent = pane
            .color
            .or(tab_color)
            .unwrap_or_else(|| self.terminal_accent(pane_id))
            .as_rgb();
        let row_background = composite_rgb(pane_accent, THEME.sidebar, TAB_COLOR_ALPHA);
        let row_text = readable_text_color(row_background);
        div()
            .id(("workspace-tab", element_key(pane_id)))
            .ml(px(indent))
            .mr(px(4.0))
            .px(px(7.0))
            .h(px(27.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(rgba(rgba_with_alpha(pane_accent, TAB_COLOR_ALPHA)))
            .border_t(if drop_above { px(2.0) } else { px(0.0) })
            .border_b(if drop_below { px(2.0) } else { px(0.0) })
            .border_color(rgb(if drop_above || drop_below {
                THEME.accent
            } else {
                row_text
            }))
            .when(selected, |element| element.border_1())
            .hover(|element| element.border_1().border_color(rgb(row_text)))
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: identity_detail.clone(),
                })
                .into()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if click_suppression_active(&mut this.suppress_tab_click_until, Instant::now()) {
                    cx.notify();
                    return;
                }
                if let Some(tab_id) = tab_id {
                    this.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
                } else {
                    this.select_workspace_tab(workspace_id, pane_id, cx);
                }
                cx.stop_propagation();
            }))
            .when_some(tab_id, |element, tab_id| {
                let drag = TabDrag {
                    workspace_id,
                    tab_id,
                    pane_id: Some(pane_id),
                    from_group,
                    title: drag_title,
                    position: Point::default(),
                };
                element
                    .on_drag(drag, |info: &TabDrag, position, _, cx| {
                        cx.new(|_| TabDrag {
                            position,
                            ..info.clone()
                        })
                    })
                    .when(!from_group, |element| {
                        element
                            .on_drag_move::<TabDrag>(cx.listener(
                                move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                                    let drag = event.drag(cx);
                                    if drag.workspace_id != workspace_id
                                        || (drag.tab_id == tab_id && !drag.from_group)
                                    {
                                        if this.tab_drop_preview.take().is_some() {
                                            cx.notify();
                                        }
                                        return;
                                    }
                                    if event.bounds.contains(&event.event.position) {
                                        this.tab_drop_preview = Some(TabDropPreview {
                                            target_tab_id: tab_id,
                                            after: event.event.position.y > event.bounds.center().y,
                                            into_group: false,
                                        });
                                        cx.stop_propagation();
                                        cx.notify();
                                    }
                                },
                            ))
                            .on_drop(cx.listener(move |this, info: &TabDrag, _, cx| {
                                if info.workspace_id == workspace_id {
                                    let after = this.tab_drop_preview.is_some_and(|preview| {
                                        preview.target_tab_id == tab_id && preview.after
                                    });
                                    if let Some(source_pane) =
                                        info.pane_id.filter(|_| info.from_group)
                                    {
                                        this.move_sidebar_pane_to_new_tab(
                                            source_pane,
                                            tab_id,
                                            after,
                                            None,
                                            cx,
                                        );
                                    } else if info.tab_id != tab_id {
                                        this.reorder_workspace_tab(info.tab_id, tab_id, after, cx);
                                    }
                                }
                                cx.stop_propagation();
                            }))
                    })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_tab_menu(pane_id, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(18.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.render_pane_identity_mark(pane, row_text, row_text)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .font_weight(if selected {
                        gpui::FontWeight::MEDIUM
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(rgb(row_text))
                    .child(identity.label),
            )
            .into_any_element()
    }

    fn render_sidebar_resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("workspace-sidebar-resize-handle")
            .relative()
            // The hit target is intentionally wider than the 2 px visual
            // divider, but stays transparent so hover never reads as a fat
            // rail or steals visual space from the workstation list.
            .w(px(SIDEBAR_RESIZE_HIT_WIDTH))
            .h_full()
            .flex_none()
            .cursor(CursorStyle::ResizeLeftRight)
            .flex()
            .justify_center()
            .bg(rgba(0x00000000))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.resizing = None;
                    this.sidebar_resize.begin(this.preferred_sidebar_width);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("workspace-sidebar-resize-visual")
                    .w(px(SIDEBAR_RESIZE_VISUAL_WIDTH))
                    .h_full()
                    .bg(rgb(THEME.border))
                    .hover(|element| element.bg(rgb(THEME.accent))),
            )
            .into_any_element()
    }

    fn render_pane_header(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let merge_preview = self.drag_hover.merges_into(active);
        let active_accent = self.terminal_accent(active).as_rgb();
        let terminal_controls = panes
            .iter()
            .find(|pane| pane.id == active)
            .is_some_and(|pane| matches!(pane.kind, PaneKind::Terminal));
        div()
            .id(("pane-tab-strip", element_key(active)))
            .h(px(PANE_HEADER_HEIGHT))
            .flex_none()
            .bg(rgb(THEME.surface))
            .border_b(if merge_preview { px(2.0) } else { px(1.0) })
            .border_color(if merge_preview {
                rgb(active_accent)
            } else {
                rgb(THEME.border)
            })
            .when(merge_preview, |element| {
                element.bg(rgba((active_accent << 8) | 0x18))
            })
            .flex()
            .items_center()
            .on_drag_move::<PaneDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        this.dragging_pane = Some(event.drag(cx).pane_id);
                        this.drag_hover.enter(DragDestination::Merge {
                            target_pane: active,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }
                },
            ))
            .on_drag_move::<WorkspaceDrag>(cx.listener(
                |this, event: &gpui::DragMoveEvent<WorkspaceDrag>, _, cx| {
                    this.dragging_workspace = Some(event.drag(cx).workspace_id);
                    this.workspace_drop_preview = None;
                    cx.notify();
                },
            ))
            .on_drop(cx.listener(move |this, info: &PaneDrag, _, cx| {
                this.move_pane_to_tab(info.pane_id, active, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .min_w(px(0.0))
                    .h_full()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .children(panes.into_iter().map(|pane| {
                        let pane_id = pane.id;
                        let identity = tab_identity_presentation(&pane);
                        let identity_detail = identity.detail.clone();
                        let secondary_label =
                            terminal_tab_secondary_label(&pane).map(str::to_owned);
                        let selected = pane_id == active;
                        let pane_accent = pane
                            .color
                            .unwrap_or_else(|| self.terminal_accent(pane_id))
                            .as_rgb();
                        let close_tooltip = format!("Close {}…", identity.label);
                        let drag = PaneDrag {
                            pane_id,
                            title: identity.label.clone(),
                            position: Point::default(),
                        };
                        div()
                            .id(("pane-tab", element_key(pane_id)))
                            .h_full()
                            .min_w(px(54.0))
                            .max_w(px(220.0))
                            .flex_shrink()
                            .overflow_hidden()
                            .pl(px(8.0))
                            .pr(px(4.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .border_t(if selected { px(2.0) } else { px(0.0) })
                            .border_r_1()
                            .border_color(if selected {
                                rgb(pane_accent)
                            } else {
                                rgb(THEME.border)
                            })
                            .when(selected, |element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.activate_tab(pane_id, cx);
                                cx.stop_propagation();
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.open_tab_menu(pane_id, event.position, cx);
                                    cx.stop_propagation();
                                }),
                            )
                            .on_drag(drag, |info: &PaneDrag, position, _, cx| {
                                cx.new(|_| PaneDrag {
                                    position,
                                    ..info.clone()
                                })
                            })
                            .child(
                                div()
                                    .id(("identity-badge", element_key(pane_id)))
                                    .flex_none()
                                    .w(px(IDENTITY_MARK_SIZE))
                                    .h(px(IDENTITY_MARK_SIZE))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| TooltipView {
                                            text: identity_detail.clone(),
                                        })
                                        .into()
                                    })
                                    .child(self.render_pane_identity_mark(
                                        &pane,
                                        if selected {
                                            THEME.foreground
                                        } else {
                                            THEME.muted
                                        },
                                        if selected { pane_accent } else { THEME.muted },
                                    )),
                            )
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex_1()
                                    .truncate()
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .font_weight(if selected {
                                        gpui::FontWeight::MEDIUM
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .text_color(if selected {
                                        rgb(THEME.foreground)
                                    } else {
                                        rgb(THEME.muted)
                                    })
                                    .child(identity.label),
                            )
                            .when_some(secondary_label, |element, label| {
                                element.child(
                                    div()
                                        .min_w(px(0.0))
                                        .flex_shrink()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .font_family("SF Mono")
                                        .text_size(px(9.5))
                                        .text_color(rgb(THEME.dim))
                                        .child(label),
                                )
                            })
                            .child(
                                div()
                                    .id(("close-tab", element_key(pane_id)))
                                    .ml(px(1.0))
                                    .flex_none()
                                    .w(px(18.0))
                                    .h(px(18.0))
                                    .rounded(px(4.0))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .font_family(".SystemUIFont")
                                    .text_sm()
                                    .line_height(px(14.0))
                                    .text_color(rgb(THEME.dim))
                                    .hover(|element| {
                                        element
                                            .bg(rgb(THEME.elevated))
                                            .text_color(rgb(THEME.foreground))
                                    })
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| TooltipView {
                                            text: close_tooltip.clone(),
                                        })
                                        .into()
                                    })
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_close(pane_id, cx);
                                        cx.stop_propagation();
                                    }))
                                    .child("×"),
                            )
                    })),
            )
            .when(browser_command_available(), |element| {
                element.child(self.pane_control(
                    active,
                    "new-browser-tab",
                    PaneControlIcon::Web,
                    "New browser in this group",
                    cx,
                    |this, _pane_id, cx| this.new_browser_tab(cx),
                ))
            })
            .when(terminal_controls, |element| {
                element
                    .child(self.pane_control(
                        active,
                        "new-tab",
                        PaneControlIcon::Add,
                        "New terminal in this group",
                        cx,
                        HhApp::new_tab_at,
                    ))
                    .child(self.pane_control(
                        active,
                        "split-right",
                        PaneControlIcon::SplitRight,
                        "Split right (⌘D)",
                        cx,
                        |this, pane_id, cx| {
                            this.split_at(pane_id, SplitAxis::Horizontal, cx);
                        },
                    ))
                    .child(self.pane_control(
                        active,
                        "split-down",
                        PaneControlIcon::SplitDown,
                        "Split down (⇧⌘D)",
                        cx,
                        |this, pane_id, cx| {
                            this.split_at(pane_id, SplitAxis::Vertical, cx);
                        },
                    ))
            })
            .into_any_element()
    }

    fn pane_control(
        &self,
        pane_id: Uuid,
        id: &'static str,
        icon: PaneControlIcon,
        tooltip: &'static str,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, Uuid, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id((id, element_key(pane_id)))
            .flex_none()
            .w(px(27.0))
            .h_full()
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(THEME.muted))
            .hover(|element| {
                element
                    .bg(rgb(THEME.elevated))
                    .text_color(rgb(THEME.foreground))
            })
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: tooltip.to_owned(),
                })
                .into()
            })
            .on_click(cx.listener(move |this, _, _, cx| handler(this, pane_id, cx)))
            .child(self.render_control_icon(icon))
            .into_any_element()
    }

    fn render_control_icon(&self, icon: PaneControlIcon) -> AnyElement {
        match icon {
            PaneControlIcon::Add => div()
                .font_family(".SystemUIFont")
                .text_base()
                .line_height(px(14.0))
                .child("+")
                .into_any_element(),
            PaneControlIcon::SplitRight | PaneControlIcon::SplitDown => {
                let vertical = matches!(icon, PaneControlIcon::SplitRight);
                div()
                    .relative()
                    .w(px(14.0))
                    .h(px(11.0))
                    .rounded(px(2.0))
                    .border_1()
                    .border_color(rgb(THEME.muted))
                    .child(
                        div()
                            .absolute()
                            .when(vertical, |element| {
                                element.left(px(6.0)).top(px(0.0)).w(px(1.0)).h_full()
                            })
                            .when(!vertical, |element| {
                                element.left(px(0.0)).top(px(4.5)).w_full().h(px(1.0))
                            })
                            .bg(rgb(THEME.muted)),
                    )
                    .into_any_element()
            }
            PaneControlIcon::Web => div()
                .relative()
                .w(px(13.0))
                .h(px(13.0))
                .rounded_full()
                .border_1()
                .border_color(rgb(THEME.muted))
                .child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(5.0))
                        .w_full()
                        .h(px(1.0))
                        .bg(rgb(THEME.muted)),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(3.5))
                        .top(px(0.0))
                        .w(px(4.0))
                        .h_full()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(THEME.muted)),
                )
                .into_any_element(),
        }
    }

    fn render_terminal(
        &self,
        panes: Vec<Pane>,
        active: Uuid,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.focused_pane == Some(active);
        let terminal_accent = self.terminal_accent(active).as_rgb();
        let metrics = self.terminal_metrics(active);
        let screen = self.screens.get(&active);
        let archived = self.archived_views.get(&active);
        let exited = self
            .pane_states
            .get(&active)
            .is_some_and(|state| state.exited);
        let drop_target = self
            .dragging_pane
            .and_then(|source| split_target_for_drag(source, &panes, active));
        let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        let tab_pane_ids = pane_ids.clone();
        let rendered_lines = if let (Some(view), Some(screen)) = (archived, screen) {
            view.page
                .lines
                .iter()
                .skip(view.first_line)
                .take(usize::from(screen.rows))
                .map(|line| plain_history_line(line))
                .enumerate()
                .map(|(row, line)| {
                    self.render_terminal_line(
                        &line,
                        TerminalLineRender {
                            row,
                            cursor: None,
                            focused,
                            pane_id: active,
                            columns: screen.columns,
                            selection: None,
                        },
                        metrics,
                        cx,
                    )
                })
                .collect::<Vec<_>>()
        } else {
            screen
                .map(|screen| {
                    screen
                        .lines
                        .iter()
                        .enumerate()
                        .map(|(row, line)| {
                            self.render_terminal_line(
                                line,
                                TerminalLineRender {
                                    row,
                                    cursor: screen.cursor,
                                    focused,
                                    pane_id: active,
                                    columns: screen.columns,
                                    selection: screen.selection,
                                },
                                metrics,
                                cx,
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        div()
            .id(("terminal", element_key(active)))
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_hidden()
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus_pane_with_snapshot(active);
                this.focus_handle.focus(window);
                cx.notify();
            }))
            .on_drop(cx.listener(move |this, info: &PaneDrag, _, cx| {
                this.swap_panes(info.pane_id, active, cx);
            }))
            .on_drag_move::<PaneDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let source = event.drag(cx).pane_id;
                    this.dragging_pane = Some(source);
                    if let Some(target_pane) = split_target_for_drag_ids(source, &pane_ids, active)
                        && let Some(placement) =
                            split_placement_at(event.event.position, event.bounds)
                    {
                        this.drag_hover.enter(DragDestination::Split {
                            target_pane,
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
                    this.dragging_pane = Some(source);
                    if let Some(target_pane) =
                        split_target_for_drag_ids(source, &tab_pane_ids, active)
                        && let Some(placement) =
                            split_placement_at(event.event.position, event.bounds)
                    {
                        this.drag_hover.enter(DragDestination::Split {
                            target_pane,
                            placement,
                        });
                    }
                    cx.notify();
                },
            ))
            .when(show_pane_header, |element| {
                element.child(self.render_pane_header(panes, active, cx))
            })
            .child(
                div()
                    .relative()
                    .min_h(px(0.0))
                    .flex_1()
                    .px(px(9.0))
                    .py(px(6.0))
                    .border_l_1()
                    .border_color(if focused {
                        rgb(terminal_accent)
                    } else {
                        rgb(THEME.terminal)
                    })
                    .font(self.terminal_font.font(false, false))
                    .text_size(px(metrics.font_size))
                    .line_height(px(metrics.line_height))
                    .text_color(rgb(THEME.foreground))
                    .children(rendered_lines)
                    .when_some(archived, |element, view| {
                        let notice = if view.page.flags.contains(HistoryPageFlags::CORRUPT) {
                            "LOCAL HISTORY · CORRUPT CHUNK · gap preserved"
                        } else if view.page.flags.contains(HistoryPageFlags::GAP_BEFORE)
                            || view.page.flags.contains(HistoryPageFlags::GAP_AFTER)
                        {
                            "LOCAL HISTORY · archive gap · live terminal unaffected"
                        } else {
                            "LOCAL HISTORY · disk-backed page · scroll down for live"
                        };
                        element.child(
                            div()
                                .absolute()
                                .top(px(3.0))
                                .right(px(8.0))
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(4.0))
                                .bg(rgb(THEME.elevated))
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(THEME.muted))
                                .child(notice),
                        )
                    })
                    .when(
                        focused
                            && self.modal.search().is_none()
                            && self.modal.pane_rename().is_none()
                            && !self.ime_preedit.is_empty(),
                        |element| {
                            let cursor = screen.and_then(|screen| screen.cursor);
                            element.when_some(cursor, |element, cursor| {
                                let span = metrics.span(cursor.column, 1);
                                element.child(
                                    div()
                                        .absolute()
                                        .left(px(span.x))
                                        .top(px(f32::from(cursor.row) * metrics.line_height))
                                        .font(self.terminal_font.font(false, false))
                                        .text_size(px(metrics.font_size))
                                        .text_color(rgb(THEME.foreground))
                                        .border_b_1()
                                        .border_color(rgb(terminal_accent))
                                        .child(self.ime_preedit.clone()),
                                )
                            })
                        },
                    )
                    .when_some(
                        self.modal.search().filter(|_| focused),
                        |element, editor| element.child(self.render_search_bar(editor)),
                    )
                    .when(exited, |element| {
                        element.child(self.render_pane_reattach_notice(active, cx))
                    })
                    .when_some(drop_target, |element, target| {
                        element.child(self.render_drop_layer(target, cx))
                    }),
            )
            .into_any_element()
    }

    /// A pane whose process exited keeps its last frame but swallows every
    /// keystroke, which is indistinguishable from a hung terminal. Say so, and
    /// offer the one-click recovery.
    fn render_pane_reattach_notice(&self, pane_id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .bottom(px(8.0))
            .left(px(8.0))
            .right(px(8.0))
            .px(px(10.0))
            .py(px(7.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .flex_1()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child("This terminal exited — input goes nowhere until it reattaches."),
            )
            .child(
                div()
                    .id(("reattach-pane", element_key(pane_id)))
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.accent))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(0xffffff))
                    .on_click(cx.listener(move |this, _, _, cx| this.reattach_pane(pane_id, cx)))
                    .child("Reattach"),
            )
            .into_any_element()
    }

    fn render_terminal_line(
        &self,
        line: &TerminalLine,
        render: TerminalLineRender,
        metrics: TerminalCellMetrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let TerminalLineRender {
            row,
            cursor,
            focused,
            pane_id,
            columns,
            selection,
        } = render;
        let mut start_column = 0_u16;
        let styled_runs = line
            .runs
            .iter()
            .map(|style| {
                let columns = style.columns;
                let element = self.render_terminal_run(style, metrics, start_column, columns);
                start_column = start_column.saturating_add(columns);
                element
            })
            .collect::<Vec<_>>();
        let cursor_column = cursor
            .filter(|cursor| usize::from(cursor.row) == row)
            .map(|cursor| cursor.column);
        let pane_accent = self.terminal_accent(pane_id).as_rgb();
        div()
            .relative()
            .h(px(metrics.line_height))
            .flex_none()
            .overflow_hidden()
            .when_some(
                selection.and_then(|selection| selection_span(selection, row, columns)),
                |element, (start, width)| {
                    let span = metrics.span(start, width);
                    element.child(
                        div()
                            .absolute()
                            .left(px(span.x))
                            .top(px(0.0))
                            .w(px(span.width))
                            .h(px(span.height))
                            .bg(rgb(THEME.selection)),
                    )
                },
            )
            .children(styled_runs)
            .when_some(cursor_column, |element, column| {
                let cursor = metrics.span(column, 1);
                element.child(
                    div()
                        .absolute()
                        .left(px(cursor.x))
                        .top(px(0.0))
                        .w(px(cursor.width))
                        .h(px(cursor.height))
                        .rounded(px(1.0))
                        .border_1()
                        .border_color(if focused {
                            rgb(pane_accent)
                        } else {
                            rgb(THEME.muted)
                        })
                        .when(focused, |cursor| cursor.bg(rgba((pane_accent << 8) | 0x30))),
                )
            })
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .size_full()
                    .child(TerminalPointerElement {
                        input: cx.entity(),
                        pane_id,
                        row: u16::try_from(row).unwrap_or(u16::MAX),
                        columns,
                        cell_width: metrics.cell_width,
                    }),
            )
            .into_any_element()
    }

    fn render_search_bar(&self, editor: &SearchEditor) -> AnyElement {
        div()
            .absolute()
            .right(px(8.0))
            .top(px(7.0))
            .w(px(280.0))
            .h(px(34.0))
            .px(px(9.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(if editor.no_match {
                rgb(THEME.danger)
            } else {
                rgb(THEME.border_strong)
            })
            .shadow_lg()
            .flex()
            .items_center()
            .gap(px(7.0))
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .child("Find")
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .font(self.terminal_font.font(false, false))
                    .child(if editor.query.is_empty() && self.ime_preedit.is_empty() {
                        "Type to search…".to_owned()
                    } else {
                        format!("{}{}", editor.query, self.ime_preedit)
                    }),
            )
            .child(if editor.no_match {
                "No match"
            } else {
                "↵ next"
            })
            .into_any_element()
    }

    fn render_terminal_run(
        &self,
        style: &TerminalRun,
        metrics: TerminalCellMetrics,
        start_column: u16,
        columns: u16,
    ) -> AnyElement {
        let bold = style.attributes.contains(TerminalAttributes::BOLD);
        let dim = style.attributes.contains(TerminalAttributes::DIM);
        let italic = style.attributes.contains(TerminalAttributes::ITALIC);
        let underline = style.attributes.contains(TerminalAttributes::UNDERLINE);
        let strikethrough = style.attributes.contains(TerminalAttributes::STRIKETHROUGH);
        let foreground = THEME.terminal_color(style.foreground, bold, dim);
        let background = THEME.terminal_color(style.background, false, false);
        let span = metrics.span(start_column, columns);
        let glyph_top = (metrics.baseline - metrics.ascent).max(0.0);
        let glyph_height = metrics.ascent + metrics.descent;
        let text = if style.text.contains('\t') {
            terminal_run_display_text(style, start_column)
        } else {
            style.text.clone()
        };
        let text_len = text.len();
        div()
            .absolute()
            .left(px(span.x))
            .top(px(0.0))
            .w(px(span.width))
            .h(px(span.height))
            .overflow_hidden()
            .when(
                style.background != TerminalColor::DefaultBackground,
                |element| element.bg(rgb(background)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(glyph_top))
                    .w_full()
                    .h(px(glyph_height))
                    .whitespace_nowrap()
                    .font(self.terminal_font.font(bold, italic))
                    .text_size(px(metrics.font_size))
                    .line_height(px(glyph_height))
                    .child(StyledText::new(text).with_runs(vec![TextRun {
                        len: text_len,
                        font: self.terminal_font.font(bold, italic),
                        color: rgb(foreground).into(),
                        background_color: None,
                        underline: underline.then_some(UnderlineStyle {
                            thickness: px(1.0),
                            color: Some(rgb(foreground).into()),
                            wavy: false,
                        }),
                        strikethrough: strikethrough.then_some(StrikethroughStyle {
                            thickness: px(1.0),
                            color: Some(rgb(foreground).into()),
                        }),
                    }])),
            )
            .into_any_element()
    }

    fn render_drop_layer(&self, target_pane: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let preview = self.drag_hover.split_for(target_pane);
        let pane_accent = self.terminal_accent(target_pane).as_rgb();
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .when_some(preview, |element, placement| {
                element.child(
                    div()
                        .absolute()
                        .border_2()
                        .border_color(rgb(pane_accent))
                        .bg(rgba((pane_accent << 8) | 0x24))
                        .when(
                            matches!(placement, DropPlacement::Left | DropPlacement::Right),
                            |element| element.w(relative(0.5)).h_full(),
                        )
                        .when(
                            matches!(placement, DropPlacement::Top | DropPlacement::Bottom),
                            |element| element.h(relative(0.5)).w_full(),
                        )
                        .when(matches!(placement, DropPlacement::Right), |element| {
                            element.right(px(0.0))
                        })
                        .when(matches!(placement, DropPlacement::Bottom), |element| {
                            element.bottom(px(0.0))
                        }),
                )
            })
            .children([
                self.render_drop_zone(target_pane, DropPlacement::Left, cx),
                self.render_drop_zone(target_pane, DropPlacement::Right, cx),
                self.render_drop_zone(target_pane, DropPlacement::Top, cx),
                self.render_drop_zone(target_pane, DropPlacement::Bottom, cx),
            ])
            .into_any_element()
    }

    fn render_drop_zone(
        &self,
        target_pane: Uuid,
        placement: DropPlacement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let placement_id = match placement {
            DropPlacement::Left => "left",
            DropPlacement::Right => "right",
            DropPlacement::Top => "top",
            DropPlacement::Bottom => "bottom",
        };
        div()
            .id((placement_id, element_key(target_pane)))
            .absolute()
            .when(matches!(placement, DropPlacement::Left), |element| {
                element
                    .left(px(0.0))
                    .top(px(0.0))
                    .w(relative(0.25))
                    .h_full()
            })
            .when(matches!(placement, DropPlacement::Right), |element| {
                element
                    .right(px(0.0))
                    .top(px(0.0))
                    .w(relative(0.25))
                    .h_full()
            })
            .when(matches!(placement, DropPlacement::Top), |element| {
                element
                    .top(px(0.0))
                    .left(relative(0.25))
                    .w(relative(0.5))
                    .h(relative(0.5))
            })
            .when(matches!(placement, DropPlacement::Bottom), |element| {
                element
                    .bottom(px(0.0))
                    .left(relative(0.25))
                    .w(relative(0.5))
                    .h(relative(0.5))
            })
            .on_drop(cx.listener(move |this, info: &PaneDrag, _, cx| {
                this.move_pane_to_split(info.pane_id, target_pane, placement, cx);
                cx.stop_propagation();
            }))
            .on_drop(cx.listener(move |this, info: &TabDrag, _, cx| {
                if let Some(source_pane) = info.pane_id {
                    this.move_pane_to_split(source_pane, target_pane, placement, cx);
                    cx.stop_propagation();
                }
            }))
            .into_any_element()
    }

    fn render_tab_menu(&self, menu: TabMenu, cx: &mut Context<Self>) -> AnyElement {
        let pane_id = menu.pane_id;
        let inline_color_picker = self
            .color_picker
            .as_ref()
            .filter(|picker| picker.target == ColorTarget::Pane(pane_id));
        let pane = self.pane_metadata(pane_id);
        let is_browser = pane
            .as_ref()
            .is_some_and(|pane| matches!(&pane.kind, PaneKind::Browser { .. }));
        let pinnable_tab = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                .filter(|tab| tab.parent_tab.is_none())
                .map(|tab| (tab.id, tab.pinned))
        });
        let workspace_id = self.workspace_id_for_pane(pane_id);
        div()
            .id(("terminal-context-menu", element_key(pane_id)))
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(232.0))
            .h_auto()
            .max_h(relative(0.72))
            .overflow_y_scroll()
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .when_some(
                workspace_id.filter(|_| browser_command_available()),
                |element, workspace_id| {
                    element.child(
                        div()
                            .id(("new-browser-from-tab-menu", element_key(pane_id)))
                            .mx(px(5.0))
                            .px(px(9.0))
                            .py(px(7.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.accent_soft)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.new_browser_tab_in(workspace_id, cx)
                            }))
                            .child("New Browser"),
                    )
                },
            )
            .when_some(pinnable_tab, |element, (tab_id, pinned)| {
                element.child(
                    div()
                        .id(("toggle-tab-pin", element_key(tab_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_tab_pinned(tab_id, !pinned, cx)
                        }))
                        .child(if pinned { "Unpin" } else { "Pin to top" }),
                )
            })
            .child(
                div()
                    .id(("rename-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_rename(pane_id, cx)))
                    .child("Rename…"),
            )
            .child(
                div()
                    .mt(px(5.0))
                    .mx(px(8.0))
                    .pt(px(7.0))
                    .border_t_1()
                    .border_color(rgb(THEME.border))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Terminal identity"),
            )
            .child(
                div()
                    .id(("select-terminal-icon", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_tab_identity_picker(pane_id, cx)
                    }))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child("Select icon")
                    .child(div().flex_1())
                    .children(pane.as_ref().map(|pane| {
                        self.render_pane_identity_mark(pane, THEME.foreground, THEME.accent)
                    }))
                    .child(if menu.identity_picker_open {
                        "⌄"
                    } else {
                        "›"
                    }),
            )
            .when(menu.identity_picker_open, |element| {
                element.child(self.render_profile_choices(pane_id, cx))
            })
            .child(
                div()
                    .id(("reset-identity-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.reset_pane_identity(pane_id, cx)),
                    )
                    .child("Reset"),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .mx(px(8.0))
                    .pt(px(7.0))
                    .border_t_1()
                    .border_color(rgb(THEME.border))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child("Terminal color"),
            )
            .child(
                div()
                    .id(("pick-terminal-color", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_color_picker(ColorTarget::Pane(pane_id), cx)
                    }))
                    .child("Pick color…"),
            )
            .when_some(inline_color_picker, |element, picker| {
                element.child(self.render_inline_color_picker(picker, "inline-terminal-color", cx))
            })
            .child(
                div()
                    .id(("close-menu", element_key(pane_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_close(pane_id, cx)))
                    .child(if is_browser {
                        "Close Browser…"
                    } else {
                        "Close Terminal…"
                    }),
            )
            .into_any_element()
    }

    fn render_profile_choices(&self, pane_id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.pane_metadata(pane_id);
        let selected = pane.as_ref().and_then(|pane| pane.profile_override);
        let selected_custom = pane.and_then(|pane| pane.custom_icon);
        let choices = std::iter::once(None).chain(TerminalProfile::ALL.into_iter().map(Some));
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(choices.enumerate().map(|(index, profile)| {
                let active = selected_custom.is_none() && selected == profile;
                let label = profile.map_or_else(
                    || "Automatic terminal icon".to_owned(),
                    |profile| profile.display_name().to_owned(),
                );
                div()
                    .id(("identity-profile", index))
                    .w(px(30.0))
                    .h(px(28.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(if active {
                        rgb(THEME.foreground)
                    } else {
                        rgb(THEME.muted)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_pane_profile(pane_id, profile, cx)
                    }))
                    .tooltip(move |_, cx| {
                        cx.new(|_| TooltipView {
                            text: label.clone(),
                        })
                        .into()
                    })
                    .children(profile.map(|profile| {
                        render_terminal_profile_mark(
                            profile,
                            if active {
                                THEME.foreground
                            } else {
                                THEME.muted
                            },
                            if active { THEME.accent } else { THEME.muted },
                        )
                    }))
                    .when(profile.is_none(), |element| element.child("A"))
            }))
            .children(self.custom_icons.iter().enumerate().map(|(index, icon)| {
                let active = selected_custom.as_deref() == Some(icon.id.as_str());
                let icon_id = icon.id.clone();
                let path = icon.path.clone();
                div()
                    .id(("custom-identity-profile", index))
                    .w(px(30.0))
                    .h(px(28.0))
                    .p(px(3.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_pane_custom_icon(pane_id, Some(icon_id.clone()), cx)
                    }))
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Saved custom image".to_owned(),
                        })
                        .into()
                    })
                    .child(
                        img(path)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(3.0)),
                    )
            }))
            .child(
                div()
                    .id(("upload-custom-identity", element_key(pane_id)))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.import_pane_custom_icon(pane_id, cx)
                        }),
                    )
                    .child("Upload image…"),
            )
            .into_any_element()
    }

    fn render_global_navigation(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut workspaces = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.workspaces.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        workspaces.sort_by_key(|workspace| (!workspace.pinned, workspace.order));
        let sidebar_visible = self.sidebar_visible;
        let navigation_hint = format!(
            "{} · {} · ⇧⌘P commands",
            THEME.name, self.terminal_font.family
        );
        let tab_scroll_to_start = self.workstation_tab_scroll.clone();
        let tab_scroll_to_end = self.workstation_tab_scroll.clone();
        let last_workspace_index = workspaces.len().saturating_sub(1);

        div()
            .id("global-workstation-navigation")
            .h(px(TITLEBAR_HEIGHT))
            .flex_none()
            // This is the actual macOS titlebar row. Keep controls clear of
            // the traffic lights while sharing their vertical alignment.
            .pl(px(MACOS_TRAFFIC_LIGHT_SAFE_INSET))
            .pr(px(10.0))
            .bg(rgb(THEME.window))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .id("toggle-workstation-sidebar")
                    .flex_none()
                    .w(px(24.0))
                    .h(px(24.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .focusable()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .in_focus(|style| style.bg(rgb(THEME.elevated)))
                    .tooltip(move |_, cx| {
                        cx.new(|_| TooltipView {
                            text: if sidebar_visible {
                                "Hide workstation sidebar (⌘B)".to_owned()
                            } else {
                                "Show workstation sidebar (⌘B)".to_owned()
                            },
                        })
                        .into()
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
                    .child(render_sidebar_toggle_icon(sidebar_visible)),
            )
            .child(
                div()
                    .w(px(1.0))
                    .h(px(18.0))
                    .flex_none()
                    .bg(rgb(THEME.border)),
            )
            .child(
                div()
                    .id("scroll-workstation-tabs-left")
                    .flex_none()
                    .w(px(20.0))
                    .h(px(24.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .focusable()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Show first workstation tabs".to_owned(),
                        })
                        .into()
                    })
                    .on_click(move |_, _, cx| {
                        tab_scroll_to_start.scroll_to_item(0);
                        cx.refresh_windows();
                    })
                    .child("‹"),
            )
            .child(
                div()
                    .id("global-workstation-tabs")
                    .min_w(px(0.0))
                    .h_full()
                    .flex_1()
                    .overflow_x_scroll()
                    .track_scroll(&self.workstation_tab_scroll)
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .children(
                        workspaces
                            .into_iter()
                            .enumerate()
                            .map(|(index, workspace)| {
                                let workspace_id = workspace.id;
                                let active = Some(workspace_id) == self.active_workspace;
                                let title = workspace.title.clone();
                                let color = self.workspace_color(workspace_id).as_rgb();
                                let tooltip_title = title.clone();
                                let shortcut = (index < 9).then(|| format!(" (⌘{})", index + 1));
                                div()
                                    .id(("global-workstation-tab", element_key(workspace_id)))
                                    .flex_none()
                                    .max_w(px(220.0))
                                    .h(px(26.0))
                                    .px(px(9.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .focusable()
                                    .when(active, |element| {
                                        element
                                            .bg(rgb(THEME.elevated))
                                            .border_1()
                                            .border_color(rgb(color))
                                    })
                                    .when(!active, |element| {
                                        element
                                            .border_1()
                                            .border_color(rgb(THEME.border))
                                            .hover(|element| element.bg(rgb(THEME.elevated)))
                                    })
                                    .in_focus(|style| style.border_color(rgb(THEME.accent)))
                                    .tooltip(move |_, cx| {
                                        cx.new(|_| TooltipView {
                                            text: format!(
                                                "Switch to {tooltip_title}{}",
                                                shortcut.as_deref().unwrap_or_default()
                                            ),
                                        })
                                        .into()
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_workspace(workspace_id, cx)
                                    }))
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.select_workspace(workspace_id, cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .w(px(7.0))
                                            .h(px(7.0))
                                            .flex_none()
                                            .rounded_full()
                                            .bg(rgb(color)),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .truncate()
                                            .whitespace_nowrap()
                                            .text_sm()
                                            .font_weight(if active {
                                                gpui::FontWeight::SEMIBOLD
                                            } else {
                                                gpui::FontWeight::NORMAL
                                            })
                                            .text_color(rgb(if active {
                                                THEME.foreground
                                            } else {
                                                THEME.muted
                                            }))
                                            .child(title),
                                    )
                            }),
                    ),
            )
            .child(
                div()
                    .id("scroll-workstation-tabs-right")
                    .flex_none()
                    .w(px(20.0))
                    .h(px(24.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .focusable()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Show more workstation tabs".to_owned(),
                        })
                        .into()
                    })
                    .on_click(move |_, _, cx| {
                        tab_scroll_to_end.scroll_to_item(last_workspace_index);
                        cx.refresh_windows();
                    })
                    .child("›"),
            )
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: navigation_hint.clone(),
                })
                .into()
            })
            .into_any_element()
    }

    fn create_menu_item(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .mx(px(5.0))
            .px(px(9.0))
            .py(px(7.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .hover(|element| element.bg(rgb(THEME.accent_soft)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.modal = Modal::None;
                handler(this, cx);
            }))
            .child(label)
            .into_any_element()
    }

    fn render_create_menu(&self, menu: CreateMenu, cx: &mut Context<Self>) -> AnyElement {
        let left = match menu.target {
            CreateMenuTarget::Global => menu.position.x,
            CreateMenuTarget::TabStrip { .. } => menu.position.x - px(232.0),
        };
        let items = match menu.target {
            CreateMenuTarget::Global => vec![
                self.create_menu_item(
                    "create-new-workstation",
                    "New Workstation",
                    cx,
                    |this, cx| {
                        this.new_workspace(cx);
                    },
                ),
                self.create_menu_item("create-new-tab", "New Tab", cx, |this, cx| {
                    if let Some(workspace_id) = this.active_workspace {
                        this.new_workspace_tab(workspace_id, cx);
                    } else {
                        cx.notify();
                    }
                }),
                self.create_menu_item("create-new-browser", "New Browser", cx, |this, cx| {
                    this.new_browser_tab(cx);
                }),
            ],
            CreateMenuTarget::TabStrip {
                workspace_id,
                target_tab,
            } => vec![
                self.create_menu_item("strip-add-project", "Add Project", cx, move |this, cx| {
                    this.begin_project_creation(workspace_id, cx);
                }),
                self.create_menu_item("strip-add-terminal", "Add Terminal", cx, move |this, cx| {
                    this.add_terminal_to_context(workspace_id, target_tab, cx);
                }),
                self.create_menu_item("strip-add-browser", "Add Browser", cx, move |this, cx| {
                    this.add_browser_to_context(workspace_id, target_tab, cx);
                }),
                self.create_menu_item("strip-add-group", "Add Group", cx, move |this, cx| {
                    this.add_group_to_context(workspace_id, target_tab, cx);
                }),
            ],
        };
        div()
            .absolute()
            .left(left)
            .top(menu.position.y)
            .w(px(232.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .children(items)
            .into_any_element()
    }

    fn render_group_icon_choices(&self, tab_id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| tab.custom_icon.clone())
        });
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(
                div()
                    .id(("automatic-group-icon", element_key(tab_id)))
                    .w(px(30.0))
                    .h(px(28.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if selected.is_none() {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.set_tab_custom_icon(tab_id, None, cx)
                        }),
                    )
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Automatic project or group icon".to_owned(),
                        })
                        .into()
                    })
                    .child("A"),
            )
            .children(self.custom_icons.iter().enumerate().map(|(index, icon)| {
                let active = selected.as_deref() == Some(icon.id.as_str());
                let icon_id = icon.id.clone();
                let path = icon.path.clone();
                div()
                    .id(("group-custom-icon", index))
                    .w(px(30.0))
                    .h(px(28.0))
                    .p(px(3.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        rgb(THEME.accent)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .bg(if active {
                        rgb(THEME.accent_soft)
                    } else {
                        rgb(THEME.surface)
                    })
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_tab_custom_icon(tab_id, Some(icon_id.clone()), cx)
                    }))
                    .child(
                        img(path)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(px(3.0)),
                    )
            }))
            .child(
                div()
                    .id(("upload-group-icon", element_key(tab_id)))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.surface))
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|element| element.border_color(rgb(THEME.foreground)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.import_tab_custom_icon(tab_id, cx)),
                    )
                    .child("Upload image…"),
            )
            .into_any_element()
    }

    fn render_group_menu(&self, menu: GroupMenu, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = menu.tab_id;
        let metadata = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot.workspaces.iter().find_map(|workspace| {
                workspace.tabs.iter().find_map(|tab| {
                    (tab.id == tab_id).then_some((
                        workspace.id,
                        tab.project_dir.is_some(),
                        tab.parent_tab.is_some(),
                        tab.pinned,
                    ))
                })
            })
        });
        let workspace_id = metadata.map(|metadata| metadata.0);
        let is_project = metadata.is_some_and(|metadata| metadata.1);
        let has_parent = metadata.is_some_and(|metadata| metadata.2);
        let pinned = metadata.is_some_and(|metadata| metadata.3);
        let inline_color_picker = self
            .color_picker
            .as_ref()
            .filter(|picker| picker.target == ColorTarget::Tab(tab_id));
        div()
            .id(("group-context-menu", element_key(tab_id)))
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(252.0))
            .h_auto()
            .max_h(relative(0.72))
            .overflow_y_scroll()
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .id(("new-group-terminal", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.new_group_terminal(tab_id, cx)),
                    )
                    .child("New terminal in group"),
            )
            .when(!has_parent, |element| {
                element.child(
                    div()
                        .id(("toggle-tab-pin", element_key(tab_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_tab_pinned(tab_id, !pinned, cx)
                        }))
                        .child(if pinned { "Unpin" } else { "Pin to top" }),
                )
            })
            .when(browser_command_available(), |element| {
                element.child(
                    div()
                        .id(("new-browser-in-group", element_key(tab_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.new_group_browser(tab_id, cx)),
                        )
                        .child("New Browser"),
                )
            })
            .when_some(
                workspace_id.filter(|_| is_project && !has_parent),
                |element, workspace_id| {
                    element.child(
                        div()
                            .id(("new-project-group", element_key(tab_id)))
                            .mx(px(5.0))
                            .px(px(9.0))
                            .py(px(7.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.accent_soft)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.new_project_group(workspace_id, tab_id, cx)
                            }))
                            .child("New Group"),
                    )
                },
            )
            .when(is_project, |element| {
                element.child(
                    div()
                        .id(("change-project-dir-menu", element_key(tab_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.begin_project_dir_edit(tab_id, cx)
                        }))
                        .child("Change Directory…"),
                )
            })
            .child(
                div()
                    .id(("rename-group-menu", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.begin_group_rename(tab_id, cx)),
                    )
                    .child("Rename group…"),
            )
            .child(
                div()
                    .id(("select-group-icon", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_group_icon_picker(tab_id, cx)
                        }),
                    )
                    .flex()
                    .items_center()
                    .child("Select icon…")
                    .child(div().flex_1())
                    .child(if menu.icon_picker_open { "⌄" } else { "›" }),
            )
            .when(menu.icon_picker_open, |element| {
                element.child(self.render_group_icon_choices(tab_id, cx))
            })
            .child(
                div()
                    .id(("pick-group-color", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_color_picker(ColorTarget::Tab(tab_id), cx)
                    }))
                    .child("Pick color…"),
            )
            .when_some(inline_color_picker, |element, picker| {
                element.child(self.render_inline_color_picker(picker, "inline-group-color", cx))
            })
            .child(
                div()
                    .id(("delete-group-menu", element_key(tab_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_tab_close(tab_id, cx)))
                    .child(if is_project {
                        "Delete project…"
                    } else {
                        "Delete group…"
                    }),
            )
            .into_any_element()
    }

    fn render_workspace_menu(&self, menu: WorkspaceMenu, cx: &mut Context<Self>) -> AnyElement {
        let workspace_id = menu.workspace_id;
        let workspace = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
        });
        let pinned = workspace.is_some_and(|workspace| workspace.pinned);
        let connection = workspace.map(|workspace| workspace.connection.clone());
        let has_working_dir = workspace.is_some_and(|workspace| workspace.working_dir.is_some());
        let tmux_scan_available = matches!(
            connection.as_ref(),
            Some(
                WorkspaceConnection::Local
                    | WorkspaceConnection::SystemSsh {
                        status: WorkspaceConnectionStatus::Connected,
                        ..
                    }
            )
        );
        let inline_color_picker = self
            .color_picker
            .as_ref()
            .filter(|picker| picker.target == ColorTarget::Workspace(workspace_id));
        div()
            .absolute()
            .left(menu.position.x)
            .top(menu.position.y)
            .w(px(232.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .id(("new-workspace-tab-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.new_workspace_tab(workspace_id, cx)),
                    )
                    .child("New Tab"),
            )
            .when(browser_command_available(), |element| {
                element.child(
                    div()
                        .id(("new-browser-tab-menu", element_key(workspace_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.new_browser_tab_in(workspace_id, cx);
                        }))
                        .child("New Browser Tab"),
                )
            })
            .child(
                div()
                    .id(("new-workspace-group-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.new_workspace_group(workspace_id, cx)
                        }),
                    )
                    .child("New Group"),
            )
            .child(
                div()
                    .id(("new-project-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_project_creation(workspace_id, cx)
                    }))
                    .child("New Project…"),
            )
            .child(
                div()
                    .id(("set-workdir-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_dir_edit(workspace_id, cx)
                    }))
                    .child("Set Working Directory…"),
            )
            .when(has_working_dir, |element| {
                element.child(
                    div()
                        .id(("clear-workdir-menu", element_key(workspace_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send(&ClientRequest::SetWorkspaceWorkingDir {
                                workspace_id,
                                working_dir: None,
                            });
                            this.refresh_state();
                            this.modal = Modal::None;
                            cx.notify();
                        }))
                        .child("Use Default Directory"),
                )
            })
            .child(
                div()
                    .id(("rename-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_rename(workspace_id, cx)
                    }))
                    .child("Rename workstation…"),
            )
            .child(
                div()
                    .id(("pin-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_workspace_pinned(workspace_id, !pinned, cx)
                    }))
                    .child(if pinned {
                        "Unpin workstation"
                    } else {
                        "Pin workstation"
                    }),
            )
            .when_some(connection, |element, connection| match connection {
                WorkspaceConnection::Local
                | WorkspaceConnection::SystemSsh {
                    status: WorkspaceConnectionStatus::Connected,
                    ..
                } => element,
                WorkspaceConnection::SystemSsh {
                    status: WorkspaceConnectionStatus::Offline,
                    ..
                } => element.child(
                    div()
                        .id(("reconnect-workspace-menu", element_key(workspace_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.ansi[2]))
                        .hover(|item| item.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reconnect_workspace(workspace_id, cx)
                        }))
                        .child("Reconnect"),
                ),
            })
            .when(tmux_scan_available, |element| {
                element.child(
                    div()
                        .id(("scan-tmux-sessions-menu", element_key(workspace_id)))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|item| item.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.scan_tmux_sessions(workspace_id, cx)
                        }))
                        .child("Scan tmux sessions…"),
                )
            })
            .child(
                div()
                    .id(("workspace-pick-color", element_key(workspace_id)))
                    .mx(px(5.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_color_picker(ColorTarget::Workspace(workspace_id), cx)
                    }))
                    .child("Pick color…"),
            )
            .when_some(inline_color_picker, |element, picker| {
                element.child(self.render_inline_color_picker(
                    picker,
                    "inline-workstation-color",
                    cx,
                ))
            })
            .child(
                div()
                    .id(("delete-workspace-menu", element_key(workspace_id)))
                    .mx(px(5.0))
                    .mt(px(4.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.danger))
                    .hover(|element| element.bg(rgb(THEME.accent_soft)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_delete(workspace_id, cx)
                    }))
                    .child("Delete workstation…"),
            )
            .into_any_element()
    }

    fn render_color_choices(
        &self,
        target: ColorTarget,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .mx(px(8.0))
            .my(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(
                self.appearance_choices()
                    .into_iter()
                    .enumerate()
                    .map(|(index, color)| {
                        let rgb_value = color.as_rgb();
                        div()
                            .id((id_prefix, index))
                            .w(px(20.0))
                            .h(px(20.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(rgb_value))
                            .border_1()
                            .border_color(rgb(THEME.border_strong))
                            .hover(|element| element.border_color(rgb(THEME.foreground)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| TooltipView {
                                    text: format!("#{rgb_value:06X}"),
                                })
                                .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_color(target, Some(color), cx)
                            }))
                    }),
            )
            .into_any_element()
    }

    /// Color pickers stay inside their owning context menu so selection does
    /// not interrupt terminal work with a second modal layer.
    fn render_inline_color_picker(
        &self,
        picker: &ColorPickerState,
        id_prefix: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = picker.target;
        div()
            .mx(px(5.0))
            .mb(px(5.0))
            .p(px(8.0))
            .rounded(px(5.0))
            .bg(rgb(THEME.surface))
            .border_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child("Recent and Harbor Night colors"),
            )
            .child(self.render_color_choices(target, id_prefix, cx))
            .child(
                div()
                    .h(px(32.0))
                    .px(px(8.0))
                    .rounded(px(5.0))
                    .bg(rgb(THEME.terminal))
                    .border_1()
                    .border_color(if picker.invalid {
                        rgb(THEME.danger)
                    } else {
                        rgb(THEME.border_strong)
                    })
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child("#")
                    .child(
                        div()
                            .when(picker.replace_on_type, |element| {
                                element.bg(rgb(THEME.selection))
                            })
                            .child(picker.hex.clone()),
                    )
                    .when(picker.invalid, |element| {
                        element.child(
                            div()
                                .ml(px(4.0))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.danger))
                                .child("Six hex digits"),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(7.0))
                    .child(
                        div()
                            .id("inline-workstation-color-default")
                            .px(px(7.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.elevated)))
                            .on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.apply_color(target, None, cx)
                                }),
                            )
                            .child("Use default"),
                    )
                    .child(
                        div()
                            .id("cancel-inline-workstation-color")
                            .px(px(7.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .hover(|element| element.bg(rgb(THEME.elevated)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.color_picker = None;
                                cx.notify();
                            }))
                            .child("Cancel"),
                    )
                    .child(
                        div()
                            .id("apply-inline-workstation-color")
                            .px(px(7.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.accent_soft))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_color_picker(cx)))
                            .child("Apply"),
                    ),
            )
            .into_any_element()
    }

    fn render_workstation_banner_setting(&self, cx: &mut Context<Self>) -> AnyElement {
        let custom = self.workstation_banner.is_some();
        let hidden = self.workstation_banner_hidden;
        let banner = self
            .workstation_banner
            .clone()
            .unwrap_or_else(workstation_banner_artwork);
        let (preview_width, preview_height) = banner_fit_size(
            SETTINGS_BANNER_PREVIEW_MAX_WIDTH - 2.0,
            SETTINGS_BANNER_PREVIEW_MAX_HEIGHT - 2.0,
            banner.aspect_ratio(),
        );
        div()
            .pt(px(4.0))
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .pt(px(6.0))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Workstation banner"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .px(px(7.0))
                                    .py(px(3.0))
                                    .rounded(px(4.0))
                                    .bg(rgb(if custom {
                                        THEME.accent_soft
                                    } else {
                                        THEME.surface
                                    }))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.foreground))
                                    .child(if custom { "Custom" } else { "Default" }),
                            )
                            .child(
                                div()
                                    .id("workstation-banner-visible")
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(if hidden {
                                        THEME.surface
                                    } else {
                                        THEME.accent_soft
                                    }))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.foreground))
                                    .hover(|element| element.bg(rgb(THEME.elevated)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_workstation_banner_visibility(cx);
                                    }))
                                    .child(if hidden { "Hidden" } else { "Shown" }),
                            ),
                    ),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child(
                        "Shown at the top of the workstation sidebar. Any aspect ratio is shown whole; the rail header matches the image and is capped at 260 px tall.",
                    ),
            )
            .child(
                div()
                    .w(px(preview_width + 2.0))
                    .h(px(preview_height + 2.0))
                    .flex_none()
                    .overflow_hidden()
                    .rounded(px(6.0))
                    .bg(rgb(THEME.terminal))
                    .border_1()
                    .border_color(rgb(THEME.border))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        img(banner.image)
                            .id("settings-workstation-banner-preview")
                            .w(px(preview_width))
                            .h(px(preview_height))
                            .object_fit(gpui::ObjectFit::Contain),
                    ),
            )
            .when(hidden, |element| {
                element.child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "Hidden from the workstation sidebar. The image stays saved for when you show it again.",
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .id("choose-workstation-banner")
                            .px(px(9.0))
                            .py(px(5.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.accent_soft))
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose_workstation_banner(cx);
                            }))
                            .child(if custom {
                                "Replace image…"
                            } else {
                                "Choose image…"
                            }),
                    )
                    .when(custom, |element| {
                        element.child(
                            div()
                                .id("reset-workstation-banner")
                                .px(px(9.0))
                                .py(px(5.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.surface))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .hover(|element| element.bg(rgb(THEME.elevated)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.reset_workstation_banner(cx);
                                }))
                                .child("Use default"),
                        )
                    }),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(
                        "PNG, JPEG, WebP, or GIF · 12 MiB maximum · copied to private local storage",
                    ),
            )
            .into_any_element()
    }

    fn render_appearance_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let appearance = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.appearance.clone())
            .unwrap_or_default();
        div()
            .id("settings-workspace-surface")
            .size_full()
            .min_h(px(0.0))
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(PANE_HEADER_HEIGHT))
                    .flex_none()
                    .px(px(10.0))
                    .bg(rgb(THEME.surface))
                    .border_b_1()
                    .border_color(rgb(THEME.border))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .w(px(22.0))
                            .text_center()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("⚙"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .id("close-appearance")
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(THEME.muted))
                            .hover(|element| {
                                element
                                    .bg(rgb(THEME.elevated))
                                    .text_color(rgb(THEME.foreground))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.modal = Modal::None;
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .id("settings-workspace-content")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .px(px(24.0))
                    .py(px(20.0))
                    .child(
                        div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Appearance"),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child("Global defaults stay independent. Terminal accents never recolor workstations, and workstation colors never recolor terminals."),
                    )
                    .child(self.render_appearance_row(
                        "Default terminal accent",
                        "Focus rail, active tab, cursor, and terminal focus treatment",
                        ColorTarget::DefaultTerminal,
                        appearance.default_terminal_accent,
                        cx,
                    ))
                    .child(self.render_appearance_row(
                        "Default workstation color",
                        "Selected workstation and workstation marker in the left rail",
                        ColorTarget::DefaultWorkspace,
                        appearance.default_workspace_color,
                        cx,
                    ))
                    .child(self.render_workstation_banner_setting(cx))
                    .child(
                        div()
                            .pt(px(2.0))
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Saved locally with session layout · no network or telemetry"),
                    )
                    .child(self.render_history_settings(cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_history_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.history_status.clone().unwrap_or(HistoryArchiveStatus {
            settings: HistorySettings::default(),
            live_scrollback_lines: 2_000,
            archived_bytes: 0,
            retained_sessions: 0,
            oldest_started_ms: None,
            dropped_bytes: 0,
            warning: None,
        });
        let settings = status.settings.clone();
        let oldest = status
            .oldest_started_ms
            .map_or_else(|| "none yet".to_owned(), format_history_date);
        let warning = history_warning_text(status.warning, status.dropped_bytes);
        let active_workspace = self.active_workspace;
        let focused_pane = self.focused_pane;
        div()
            .pt(px(4.0))
            .border_t_1()
            .border_color(rgb(THEME.border))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .pt(px(6.0))
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("History Storage"),
                    )
                    .child(
                        div()
                            .id("history-enabled")
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(if settings.enabled {
                                THEME.accent_soft
                            } else {
                                THEME.surface
                            }))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.mutate_history_settings(
                                    |settings| settings.enabled = !settings.enabled,
                                    cx,
                                );
                            }))
                            .child(if settings.enabled {
                                "On · local only"
                            } else {
                                "Off"
                            }),
                    ),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child(format!(
                        "Live memory: {} lines · Local archive: {} · {} sessions · oldest {}",
                        status.live_scrollback_lines,
                        format_bytes(status.archived_bytes),
                        status.retained_sessions,
                        oldest
                    )),
            )
            .when_some(warning, |element, warning| {
                element.child(
                    div()
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(5.0))
                        .bg(rgb(THEME.surface))
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.danger))
                        .child(warning),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(history_label("Retention"))
                    .children(
                        [
                            ("Forever", HistoryRetention::Indefinite),
                            ("7d", HistoryRetention::Days { days: 7 }),
                            ("30d", HistoryRetention::Days { days: 30 }),
                            ("90d", HistoryRetention::Days { days: 90 }),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, retention))| {
                            let selected = settings.retention == retention;
                            div()
                                .id(("history-retention", index))
                                .px(px(7.0))
                                .py(px(3.0))
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .bg(rgb(if selected {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mutate_history_settings(
                                        |settings| settings.retention = retention,
                                        cx,
                                    );
                                }))
                                .child(label)
                        }),
                    )
                    .child(self.render_history_custom_field(
                        HistoryEditField::RetentionDays,
                        "Custom days",
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(history_label("Quota"))
                    .children(
                        [
                            ("1 GiB", 1_u64),
                            ("5 GiB", 5),
                            ("10 GiB", 10),
                            ("50 GiB", 50),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, gib))| {
                            let bytes = gib * 1024 * 1024 * 1024;
                            let selected = settings.quota_bytes == bytes;
                            div()
                                .id(("history-quota", index))
                                .px(px(7.0))
                                .py(px(3.0))
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .bg(rgb(if selected {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_xs()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mutate_history_settings(
                                        |settings| settings.quota_bytes = bytes,
                                        cx,
                                    );
                                }))
                                .child(label)
                        }),
                    )
                    .child(self.render_history_custom_field(
                        HistoryEditField::QuotaGib,
                        "Custom GiB",
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(history_label("At capacity"))
                    .child(
                        div()
                            .id("history-pause-policy")
                            .px(px(7.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .bg(rgb(
                                if settings.cleanup_policy == HistoryCleanupPolicy::PauseWhenFull {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                },
                            ))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mutate_history_settings(
                                    |settings| {
                                        settings.cleanup_policy =
                                            HistoryCleanupPolicy::PauseWhenFull;
                                    },
                                    cx,
                                );
                            }))
                            .child("Pause + warn (safe)"),
                    )
                    .child(
                        div()
                            .id("history-delete-oldest-policy")
                            .px(px(7.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .bg(rgb(
                                if settings.cleanup_policy == HistoryCleanupPolicy::DeleteOldest {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                },
                            ))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.mutate_history_settings(
                                    |settings| {
                                        settings.cleanup_policy =
                                            HistoryCleanupPolicy::DeleteOldest;
                                    },
                                    cx,
                                );
                            }))
                            .child("Auto-delete oldest (opt-in)"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(history_label("Clear"))
                    .when_some(focused_pane, |element, pane_id| {
                        element.child(self.render_clear_history_button(
                            "Terminal",
                            HistoryClearScope::Terminal { pane_id },
                            cx,
                        ))
                    })
                    .when_some(active_workspace, |element, workspace_id| {
                        element.child(self.render_clear_history_button(
                            "Workstation",
                            HistoryClearScope::Workspace { workspace_id },
                            cx,
                        ))
                    })
                    .child(self.render_clear_history_button(
                        "All history",
                        HistoryClearScope::All,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_right()
                            .font_family("SF Mono")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child("Future output only · older sessions cannot be recovered"),
                    ),
            )
            .into_any_element()
    }

    fn render_history_custom_field(
        &self,
        field: HistoryEditField,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self
            .history_editor
            .as_ref()
            .filter(|editor| editor.field == field);
        let label = editor.map_or_else(
            || placeholder.to_owned(),
            |editor| {
                if editor.invalid {
                    format!("{} · invalid", editor.text)
                } else {
                    format!("{} ↵", editor.text)
                }
            },
        );
        div()
            .id(match field {
                HistoryEditField::RetentionDays => "custom-retention",
                HistoryEditField::QuotaGib => "custom-quota",
            })
            .px(px(7.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if editor.is_some() {
                THEME.accent
            } else {
                THEME.border
            }))
            .font_family("SF Mono")
            .text_xs()
            .text_color(rgb(if editor.is_some_and(|editor| editor.invalid) {
                THEME.danger
            } else {
                THEME.muted
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.begin_history_edit(field, cx)))
            .child(label)
            .into_any_element()
    }

    fn render_clear_history_button(
        &self,
        label: &'static str,
        scope: HistoryClearScope,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let confirming = self.history_clear_confirmation == Some(scope);
        div()
            .id(("clear-history", history_scope_key(scope)))
            .px(px(7.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .border_1()
            .border_color(rgb(if confirming {
                THEME.danger
            } else {
                THEME.border
            }))
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(if confirming {
                THEME.danger
            } else {
                THEME.muted
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.clear_history(scope, cx)))
            .child(if confirming {
                format!("Confirm {label}")
            } else {
                label.to_owned()
            })
            .into_any_element()
    }

    fn render_appearance_row(
        &self,
        label: &'static str,
        description: &'static str,
        target: ColorTarget,
        color: AppearanceColor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rgb_value = color.as_rgb();
        div()
            .p(px(12.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.surface))
            .border_1()
            .border_color(rgb(THEME.border))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .w(px(28.0))
                    .h(px(28.0))
                    .rounded(px(7.0))
                    .bg(rgb(rgb_value))
                    .border_1()
                    .border_color(rgb(THEME.border_strong)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(THEME.foreground))
                            .child(label),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .child(description),
                    ),
            )
            .child(
                div()
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.dim))
                    .child(format!("#{rgb_value:06X}")),
            )
            .child(
                div()
                    .id(match target {
                        ColorTarget::DefaultTerminal => "pick-default-terminal",
                        ColorTarget::DefaultWorkspace => "pick-default-workspace",
                        ColorTarget::Pane(_) => "pick-pane",
                        ColorTarget::Workspace(_) => "pick-workspace",
                        ColorTarget::Tab(_) => "pick-tab",
                    })
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.border_color(rgb(rgb_value)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_color_picker(target, cx)))
                    .child("Pick color…"),
            )
            .into_any_element()
    }

    fn render_color_picker(&self, picker: &ColorPickerState, cx: &mut Context<Self>) -> AnyElement {
        let target = picker.target;
        let (title, can_reset) = match target {
            ColorTarget::DefaultTerminal => ("Pick default terminal accent", false),
            ColorTarget::DefaultWorkspace => ("Pick default workstation color", false),
            ColorTarget::Pane(_) => ("Pick terminal color", true),
            ColorTarget::Workspace(_) => ("Pick workstation color", true),
            ColorTarget::Tab(_) => ("Pick group or project color", true),
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0faa))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(340.0))
                    .p(px(16.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(11.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(title),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .child("Recent colors first, followed by Harbor Night presets."),
                    )
                    .child(self.render_color_choices(target, "color-picker", cx))
                    .child(
                        div()
                            .h(px(36.0))
                            .px(px(10.0))
                            .rounded(px(6.0))
                            .bg(rgb(THEME.terminal))
                            .border_1()
                            .border_color(if picker.invalid {
                                rgb(THEME.danger)
                            } else {
                                rgb(THEME.border_strong)
                            })
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .font_family("SF Mono")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child("#")
                            .child(
                                div()
                                    .when(picker.replace_on_type, |element| {
                                        element.bg(rgb(THEME.selection))
                                    })
                                    .child(picker.hex.clone()),
                            )
                            .child("│")
                            .when(picker.invalid, |element| {
                                element.child(
                                    div()
                                        .ml(px(6.0))
                                        .font_family(".SystemUIFont")
                                        .text_xs()
                                        .text_color(rgb(THEME.danger))
                                        .child("Enter six hex digits"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .when(can_reset, |element| {
                                element.child(
                                    div()
                                        .id("picker-use-default")
                                        .px(px(11.0))
                                        .py(px(7.0))
                                        .rounded(px(5.0))
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(rgb(THEME.foreground))
                                        .hover(|element| element.bg(rgb(THEME.surface)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.apply_color(target, None, cx)
                                        }))
                                        .child("Use default"),
                                )
                            })
                            .child(
                                div()
                                    .id("cancel-color-picker")
                                    .px(px(11.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.surface)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.color_picker = None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("apply-color-picker")
                                    .px(px(11.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(THEME.accent_soft))
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .hover(|element| element.bg(rgb(THEME.selection)))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.submit_color_picker(cx)),
                                    )
                                    .child("Apply"),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn confirm_dialog(
        &self,
        body: AnyElement,
        spec: DialogSpec,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let DialogSpec {
            title,
            confirm_label,
            confirm_tone,
            confirm_id,
            action,
        } = spec;
        let confirm_background = match confirm_tone {
            DialogTone::Accent => THEME.accent,
            DialogTone::Danger => THEME.danger,
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(440.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(title),
                    )
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-dialog")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .hover(|element| element.bg(rgb(THEME.surface)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.modal = Modal::None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id(confirm_id)
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(confirm_background))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(move |this, _, _, cx| match action {
                                        DialogAction::RenamePane => this.submit_rename(cx),
                                        DialogAction::RenameWorkspace => {
                                            this.submit_workspace_rename(cx);
                                        }
                                        DialogAction::RenameTab => {
                                            this.submit_group_rename(cx);
                                        }
                                        DialogAction::DeleteWorkspace => {
                                            this.confirm_workspace_delete(cx);
                                        }
                                        DialogAction::DisconnectWorkspace => {
                                            this.confirm_workspace_disconnect(cx);
                                        }
                                        DialogAction::ClosePane => this.confirm_close(cx),
                                        DialogAction::ConfirmDirEditor => {
                                            this.submit_dir_editor(cx);
                                        }
                                        DialogAction::CloseTab => this.confirm_tab_close(cx),
                                    }))
                                    .child(confirm_label),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_rename_dialog(&self, editor: &RenameEditor, cx: &mut Context<Self>) -> AnyElement {
        let body = div()
            .id("terminal-rename-input")
            .track_focus(&self.focus_handle)
            .h(px(36.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.terminal))
            .border_1()
            .border_color(rgb(THEME.accent))
            .flex()
            .items_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus_handle.focus(window);
                    cx.stop_propagation();
                }),
            )
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .child(
                div()
                    .when(editor.replace_on_type, |element| {
                        element.bg(rgb(THEME.selection))
                    })
                    .child(format!("{}{}", editor.value, self.ime_preedit)),
            )
            .child("│")
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: "Rename terminal".to_owned(),
                confirm_label: "Rename",
                confirm_tone: DialogTone::Accent,
                confirm_id: "save-rename",
                action: DialogAction::RenamePane,
            },
            cx,
        )
    }

    fn render_group_rename_dialog(
        &self,
        editor: &GroupRenameEditor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .id("group-rename-input")
            .track_focus(&self.focus_handle)
            .h(px(36.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.terminal))
            .border_1()
            .border_color(rgb(THEME.accent))
            .flex()
            .items_center()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .child(
                div()
                    .when(editor.replace_on_type, |element| {
                        element.bg(rgb(THEME.selection))
                    })
                    .child(format!("{}{}", editor.value, self.ime_preedit)),
            )
            .child("│")
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: "Rename group".to_owned(),
                confirm_label: "Rename",
                confirm_tone: DialogTone::Accent,
                confirm_id: "save-group-rename",
                action: DialogAction::RenameTab,
            },
            cx,
        )
    }

    fn render_workspace_creation_dialog(
        &self,
        dialog: &WorkspaceCreationDialog,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = dialog.kind;
        let field = dialog.field;
        let destination = dialog.destination.text.clone();
        let error = dialog.error.clone();
        let name_input_focus =
            self.workspace_input_focus[WorkspaceCreationField::Name.index()].clone();
        let destination_input_focus =
            self.workspace_input_focus[WorkspaceCreationField::Destination.index()].clone();
        let content = match dialog.step {
            WorkspaceCreationStep::Details => div()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(THEME.foreground))
                        .child("New Workstation"),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("new-workspace-local")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .border_1()
                                .border_color(rgb(if kind == WorkspaceCreationKind::Local {
                                    THEME.accent
                                } else {
                                    THEME.border_strong
                                }))
                                .bg(rgb(if kind == WorkspaceCreationKind::Local {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_sm()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(dialog) = this.modal.workspace_creation_mut() {
                                        dialog.kind = WorkspaceCreationKind::Local;
                                        dialog.field = WorkspaceCreationField::Name;
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                }))
                                .child("Local shell"),
                        )
                        .child(
                            div()
                                .id("new-workspace-ssh")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .border_1()
                                .border_color(rgb(if kind == WorkspaceCreationKind::SystemSsh {
                                    THEME.accent
                                } else {
                                    THEME.border_strong
                                }))
                                .bg(rgb(if kind == WorkspaceCreationKind::SystemSsh {
                                    THEME.accent_soft
                                } else {
                                    THEME.surface
                                }))
                                .text_sm()
                                .text_color(rgb(THEME.foreground))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(dialog) = this.modal.workspace_creation_mut() {
                                        dialog.kind = WorkspaceCreationKind::SystemSsh;
                                        dialog.field = WorkspaceCreationField::Destination;
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                }))
                                .child("System SSH"),
                        ),
                )
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child("Workstation name (optional)"),
                )
                .child(
                    div()
                        .id("workspace-name-input")
                        .track_focus(&name_input_focus)
                        .h(px(36.0))
                        .px(px(10.0))
                        .rounded(px(6.0))
                        .bg(rgb(THEME.terminal))
                        .border_1()
                        .border_color(rgb(if field == WorkspaceCreationField::Name {
                            THEME.accent
                        } else {
                            THEME.border_strong
                        }))
                        .overflow_hidden()
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.focus_workspace_creation_field(
                                WorkspaceCreationField::Name,
                                Some(event.position),
                                event.modifiers.shift,
                                event.click_count,
                                window,
                            );
                            cx.notify();
                            }),
                        )
                        .child(WorkspaceTextInputElement {
                            input: cx.entity(),
                            field: WorkspaceCreationField::Name,
                            placeholder: "Workstation name",
                        }),
                )
                .when(kind == WorkspaceCreationKind::SystemSsh, |element| {
                    element
                        .child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child("SSH destination or exact ssh command"),
                        )
                        .child(
                            div()
                                .id("workspace-ssh-input")
                                .track_focus(&destination_input_focus)
                                .h(px(36.0))
                                .px(px(10.0))
                                .rounded(px(6.0))
                                .bg(rgb(THEME.terminal))
                                .border_1()
                                .border_color(rgb(
                                    if field == WorkspaceCreationField::Destination {
                                        THEME.accent
                                    } else {
                                        THEME.border_strong
                                    },
                                ))
                                .overflow_hidden()
                                .cursor_pointer()
                                .flex()
                                .items_center()
                                .font_family("SF Mono")
                                .text_sm()
                                .text_color(rgb(THEME.foreground))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    this.focus_workspace_creation_field(
                                        WorkspaceCreationField::Destination,
                                        Some(event.position),
                                        event.modifiers.shift,
                                        event.click_count,
                                        window,
                                    );
                                    cx.notify();
                                    }),
                                )
                                .child(WorkspaceTextInputElement {
                                    input: cx.entity(),
                                    field: WorkspaceCreationField::Destination,
                                    placeholder: "ssh user@host-or-alias",
                                }),
                        )
                })
                .when(kind == WorkspaceCreationKind::SystemSsh, |element| {
                    element.child(
                        div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "The workstation connects immediately after confirmation and saves only its name, destination, pin/order, and offline/connected intent locally. System OpenSSH keeps authority over config, agent, keys, proxies, and known_hosts. Harness Harlot stores no credentials or SSH config contents.",
                        ),
                    )
                })
                .when_some(error, |element, message| {
                    element.child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.danger))
                            .child(message),
                    )
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("cancel-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.modal = Modal::None;
                                    cx.notify();
                                }))
                                .child("Cancel"),
                        )
                        .child(
                            div()
                                .id("submit-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .text_color(rgb(0xffffff))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit_workspace_creation(cx)
                                }))
                                .child(if kind == WorkspaceCreationKind::SystemSsh {
                                    "Review connection"
                                } else {
                                    "Create workstation"
                                }),
                        ),
                )
                .into_any_element(),
            WorkspaceCreationStep::ConfirmSsh => div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(THEME.foreground))
                        .child(format!("Connect and save {destination}?")),
                )

                .child(
                    div()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.muted))
                        .child(
                            "This starts the installed OpenSSH client now and saves safe workstation metadata locally for later reconnect. Harness Harlot adds no SSH options, stores no credentials, and does not change your config, agent, forwarding, or host-key policy.",
                        ),
                )
                .when_some(error, |element, message| {
                    element.child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.danger))
                            .child(message),
                    )
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("back-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(dialog) = this.modal.workspace_creation_mut() {
                                        dialog.step = WorkspaceCreationStep::Details;
                                        dialog.error = None;
                                    }
                                    cx.notify();
                                }))
                                .child("Back"),
                        )
                        .child(
                            div()
                                .id("confirm-workspace-create")
                                .px(px(12.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .text_color(rgb(0xffffff))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit_workspace_creation(cx)
                                }))
                                .child("Connect and save"),
                        ),
                )
                .into_any_element(),
        };

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(520.0))
                    .p(px(18.0))
                    .rounded(px(10.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_workspace_rename_dialog(
        &self,
        editor: &WorkspaceRenameEditor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .h(px(36.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .bg(rgb(THEME.terminal))
            .border_1()
            .border_color(rgb(THEME.accent))
            .flex()
            .items_center()
            .text_sm()
            .text_color(rgb(THEME.foreground))
            .child(editor.value.clone())
            .child("│")
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: "Rename workstation".to_owned(),
                confirm_label: "Rename",
                confirm_tone: DialogTone::Accent,
                confirm_id: "save-workspace-rename",
                action: DialogAction::RenameWorkspace,
            },
            cx,
        )
    }

    fn render_dir_editor_dialog(&self, editor: &DirEditor, cx: &mut Context<Self>) -> AnyElement {
        let (title, confirm_label) = match editor.target {
            DirEditorTarget::WorkspaceDefault(_) => ("Set working directory", "Set"),
            DirEditorTarget::NewProject(_) => ("New project", "Create"),
            DirEditorTarget::ProjectDir(_) => ("Change project directory", "Set"),
        };
        let body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .h(px(36.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .bg(rgb(THEME.terminal))
                    .border_1()
                    .border_color(rgb(THEME.accent))
                    .flex()
                    .items_center()
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child(editor.value.clone())
                    .child("│"),
            )
            .when(!editor.suggestions.is_empty(), |element| {
                element.child(
                    div()
                        .font_family("SF Mono")
                        .text_xs()
                        .text_color(rgb(THEME.dim))
                        .child(editor.suggestions.join("  ")),
                )
            })
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: title.to_owned(),
                confirm_label,
                confirm_tone: DialogTone::Accent,
                confirm_id: "confirm-dir-editor",
                action: DialogAction::ConfirmDirEditor,
            },
            cx,
        )
    }

    fn render_workspace_delete_dialog(
        &self,
        confirmation: &WorkspaceDeleteConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = if confirmation.active_terminal_count == 0 {
            "This removes the saved workstation metadata from this machine. No active terminal process will be ended.".to_owned()
        } else {
            format!(
                "This permanently removes the workstation and ends {} active terminal process{}. Disconnecting is the non-destructive choice for a saved SSH workstation.",
                confirmation.active_terminal_count,
                if confirmation.active_terminal_count == 1 {
                    ""
                } else {
                    "es"
                }
            )
        };
        let body = div()
            .text_sm()
            .text_color(rgb(THEME.muted))
            .child(message)
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Delete workstation {}?", confirmation.title),
                confirm_label: "Delete workstation",
                confirm_tone: DialogTone::Danger,
                confirm_id: "confirm-workspace-delete",
                action: DialogAction::DeleteWorkspace,
            },
            cx,
        )
    }
    fn render_tab_close_dialog(
        &self,
        confirmation: &TabCloseConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = if confirmation.is_project {
            "project"
        } else {
            "group"
        };
        let child_detail = if confirmation.child_count == 0 {
            String::new()
        } else {
            format!(
                " Its {} nested group{} will also be removed.",
                confirmation.child_count,
                if confirmation.child_count == 1 {
                    ""
                } else {
                    "s"
                }
            )
        };
        let body = div()
            .text_sm()
            .text_color(rgb(THEME.muted))
            .child(format!(
                "This permanently removes the {kind} and ends {} terminal process{}.{}",
                confirmation.terminal_count,
                if confirmation.terminal_count == 1 {
                    ""
                } else {
                    "es"
                },
                child_detail,
            ))
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Delete {kind} {}?", confirmation.title),
                confirm_label: if confirmation.is_project {
                    "Delete project"
                } else {
                    "Delete group"
                },
                confirm_tone: DialogTone::Danger,
                confirm_id: "confirm-tab-close",
                action: DialogAction::CloseTab,
            },
            cx,
        )
    }

    fn render_workspace_connection_info(
        &self,
        info: &WorkspaceConnectionInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let connection = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == info.workspace_id)
                .and_then(|workspace| match &workspace.connection {
                    WorkspaceConnection::SystemSsh {
                        destination,
                        status: WorkspaceConnectionStatus::Connected,
                    } => Some((workspace.title.clone(), destination.clone())),
                    WorkspaceConnection::Local
                    | WorkspaceConnection::SystemSsh {
                        status: WorkspaceConnectionStatus::Offline,
                        ..
                    } => None,
                })
        });
        let Some((title, destination)) = connection else {
            return div().into_any_element();
        };
        let workspace_id = info.workspace_id;
        div()
            .absolute()
            .left(info.position.x)
            .top(info.position.y)
            .w(px(260.0))
            .p(px(12.0))
            .rounded(px(7.0))
            .bg(rgb(THEME.elevated))
            .border_1()
            .border_color(rgb(THEME.border_strong))
            .shadow_lg()
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .child(title),
            )
            .child(
                div()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child("Connected with system OpenSSH"),
            )
            .child(
                div()
                    .p(px(8.0))
                    .rounded(px(5.0))
                    .bg(rgb(THEME.terminal))
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child(destination),
            )
            .child(
                div()
                    .id(("disconnect-workspace-from-info", element_key(workspace_id)))
                    .px(px(9.0))
                    .py(px(6.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.surface))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .hover(|element| element.border_color(rgb(THEME.accent)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_workspace_disconnect(workspace_id, cx)
                    }))
                    .child("Disconnect…"),
            )
            .into_any_element()
    }

    fn render_tmux_session_picker(
        &self,
        picker: &TmuxSessionPicker,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scope = match &picker.scope {
            TmuxScanScope::Local => "this Mac".to_owned(),
            TmuxScanScope::SystemSsh { destination } => format!("SSH workstation {destination}"),
        };
        let selected_count = picker.selected_session_ids.len();
        let can_open = selected_count > 0;
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x090b0f88))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .child(
                div()
                    .w(px(460.0))
                    .max_h(px(520.0))
                    .p(px(18.0))
                    .rounded(px(9.0))
                    .bg(rgb(THEME.elevated))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Open tmux sessions"),
                    )
                    .child(
                        div()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.muted))
                            .child(format!(
                                "Scanned metadata only on {scope}. One tab is opened per selected session and attaches like `tmux attach`."
                            )),
                    )
                    .when(picker.no_server, |element| {
                        element.child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .child("No tmux server is running for this scope."),
                        )
                    })
                    .when_some(picker.status.clone(), |element, status| {
                        element.child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .child(status),
                        )
                    })
                    .when_some(picker.error.clone(), |element, error| {
                        element.child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.danger))
                                .child(error),
                        )
                    })
                    .when(!picker.sessions.is_empty(), |element| {
                        element.child(
                            div()
                                .flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    div()
                                        .font_family(".SystemUIFont")
                                        .text_xs()
                                        .text_color(rgb(THEME.muted))
                                        .child(format!("{selected_count} session(s) selected")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(10.0))
                                        .child(
                                            div()
                                                .id("select-all-tmux-sessions")
                                                .cursor_pointer()
                                                .font_family(".SystemUIFont")
                                                .text_xs()
                                                .text_color(rgb(THEME.accent))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.mutate_tmux_selection(
                                                        TmuxSelectionChange::All,
                                                        cx,
                                                    );
                                                }))
                                                .child("Select All"),
                                        )
                                        .child(
                                            div()
                                                .id("clear-all-tmux-sessions")
                                                .cursor_pointer()
                                                .font_family(".SystemUIFont")
                                                .text_xs()
                                                .text_color(rgb(THEME.accent))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.mutate_tmux_selection(
                                                        TmuxSelectionChange::None,
                                                        cx,
                                                    );
                                                }))
                                                .child("Clear All"),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("tmux-session-list")
                            .min_h(px(0.0))
                            .flex_1()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .children(picker.sessions.iter().enumerate().map(|(index, session)| {
                                let session_id = session.id.clone();
                                let is_open = picker.is_open(&session.id);
                                let is_selected =
                                    picker.selected_session_ids.contains(&session.id);
                                div()
                                    .id(("tmux-session", index))
                                    .px(px(10.0))
                                    .py(px(8.0))
                                    .rounded(px(6.0))
                                    .when(!is_open, |element| element.cursor_pointer())
                                    .border_1()
                                    .border_color(rgb(if is_selected {
                                        THEME.accent
                                    } else {
                                        THEME.border_strong
                                    }))
                                    .bg(rgb(if is_selected {
                                        THEME.accent_soft
                                    } else {
                                        THEME.surface
                                    }))
                                    .when(!is_open, |element| {
                                        element.on_click(cx.listener(move |this, _, _, cx| {
                                            this.mutate_tmux_selection(
                                                TmuxSelectionChange::Session(session_id.clone()),
                                                cx,
                                            );
                                        }))
                                    })
                                    .child(
                                        div()
                                            .font_family(".SystemUIFont")
                                            .text_sm()
                                            .text_color(rgb(if is_open {
                                                THEME.muted
                                            } else {
                                                THEME.foreground
                                            }))
                                            .child(format!(
                                                "{}{}",
                                                if is_selected { "✓ " } else { "" },
                                                session.name
                                            )),
                                    )
                                    .child(
                                        div()
                                            .mt(px(2.0))
                                            .font_family(".SystemUIFont")
                                            .text_xs()
                                            .text_color(rgb(THEME.muted))
                                            .child(if is_open {
                                                "Already open in a tab".to_owned()
                                            } else {
                                                format!(
                                                    "{} window(s) · {}",
                                                    session.windows,
                                                    if session.attached_clients == 0 {
                                                        "detached".to_owned()
                                                    } else {
                                                        format!(
                                                            "{} attached",
                                                            session.attached_clients
                                                        )
                                                    }
                                                )
                                            }),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("cancel-tmux-session-picker")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(THEME.muted))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.modal = Modal::None;
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("open-selected-tmux-sessions")
                                    .px(px(12.0))
                                    .py(px(7.0))
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .bg(rgb(if can_open {
                                        THEME.accent
                                    } else {
                                        THEME.border_strong
                                    }))
                                    .text_sm()
                                    .text_color(rgb(0xffffff))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_selected_tmux_sessions(cx);
                                    }))
                                    .child(format!("Open {selected_count} selected session(s)")),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_workspace_disconnect_dialog(
        &self,
        confirmation: &WorkspaceDisconnectConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child(
                        "This closes the active system OpenSSH terminal. The saved workstation stays available for reconnect.",
                    ),
            )
            .child(
                div()
                    .p(px(8.0))
                    .rounded(px(5.0))
                    .bg(rgb(THEME.terminal))
                    .font_family("SF Mono")
                    .text_xs()
                    .text_color(rgb(THEME.foreground))
                    .child(confirmation.destination.clone()),
            )
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Disconnect {}?", confirmation.title),
                confirm_label: "Disconnect",
                confirm_tone: DialogTone::Accent,
                confirm_id: "confirm-workspace-disconnect",
                action: DialogAction::DisconnectWorkspace,
            },
            cx,
        )
    }

    fn render_close_dialog(
        &self,
        confirmation: &CloseConfirmation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = match (confirmation.is_browser, confirmation.leaves_workspace_empty) {
            (true, true) => {
                "This will close the last browser and leave the saved workstation empty. You can open a new terminal or browser from its empty state."
            }
            (true, false) => "This permanently closes this browser tab. Other tabs stay open.",
            (false, true) => {
                "This will terminate the last terminal and leave the saved workstation empty. You can open a new terminal from its empty state."
            }
            (false, false) => {
                "This will terminate this terminal and its running shell process. Other terminal tabs stay open."
            }
        };
        let body = div()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.muted))
            .child(message)
            .into_any_element();
        self.confirm_dialog(
            body,
            DialogSpec {
                title: format!("Close {}?", confirmation.title),
                confirm_label: if confirmation.is_browser {
                    "Close Browser"
                } else {
                    "Close Terminal"
                },
                confirm_tone: DialogTone::Danger,
                confirm_id: "confirm-close",
                action: DialogAction::ClosePane,
            },
            cx,
        )
    }
    #[cfg(all(target_os = "macos", feature = "browser"))]
    fn ensure_browser_view(
        &mut self,
        pane_id: Uuid,
        url: &str,
        width: f32,
        height: f32,
    ) -> anyhow::Result<()> {
        if !self.browser_runtime_initialized {
            let cache_dir = hh_protocol::state_directory()
                .ok_or_else(|| anyhow::anyhow!("app state directory is unavailable"))?
                .join("browser-cache");
            std::fs::create_dir_all(&cache_dir)
                .context("create Chromium browser cache directory")?;
            hh_cef_view::init_runtime(&cache_dir)?;
            self.browser_runtime_initialized = true;
            self.browser_runtime_error = None;
        }
        if !self.browser_views.contains_key(&pane_id) {
            let parent_view = self
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
            }));
            let address_state = Rc::clone(&shared);
            let title_state = Rc::clone(&shared);
            let favicon_state = Rc::clone(&shared);
            let loading_state = Rc::clone(&shared);
            let callbacks = hh_cef_view::Callbacks {
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
            };
            let browser = Rc::new(hh_cef_view::BrowserPane::create(
                parent_view,
                hh_cef_view::BrowserRect {
                    x: 0.0,
                    y: 0.0,
                    width: width.max(1.0),
                    height: (height - PANE_HEADER_HEIGHT - 38.0).max(1.0),
                },
                url,
                callbacks,
            )?);
            self.browser_views.insert(
                pane_id,
                BrowserPaneView {
                    pane: browser,
                    shared,
                    last_snapshot_url: url.to_owned(),
                    synced_url: url.to_owned(),
                    synced_title: None,
                    focused: false,
                    pending_state: None,
                    in_flight_state: None,
                },
            );
        }
        if let Some(view) = self.browser_views.get_mut(&pane_id) {
            if view.synced_url != url {
                view.pane.navigate(url);
            }
            url.clone_into(&mut view.last_snapshot_url);
            url.clone_into(&mut view.synced_url);
        }
        Ok(())
    }
    #[cfg(all(target_os = "macos", feature = "browser"))]
    fn sync_browser_view_presentation(&mut self, visible: &HashSet<Uuid>) {
        let content_visible = matches!(self.modal, Modal::None) && self.dragging_pane.is_none();
        let should_focus =
            content_visible && self.window_active && self.browser_url_editor.is_none();
        for (view_id, view) in &mut self.browser_views {
            let visible_now = content_visible && visible.contains(view_id);
            let focused_now = visible_now && should_focus && Some(*view_id) == self.focused_pane;
            view.pane.set_visible(visible_now);
            if view.focused != focused_now {
                view.pane.focus(focused_now);
                view.focused = focused_now;
            }
        }
    }

    #[cfg(all(target_os = "macos", feature = "browser"))]
    fn sync_browser_callback_state(&mut self) -> bool {
        let browser_ids = self
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
                    .filter_map(|pane| {
                        matches!(pane.kind, PaneKind::Browser { .. }).then_some(pane.id)
                    })
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        self.browser_views
            .retain(|pane_id, _| browser_ids.contains(pane_id));

        let updates = self
            .browser_views
            .iter()
            .filter_map(|(pane_id, view)| {
                let mut shared = view.shared.borrow_mut();
                if !shared.dirty {
                    return None;
                }
                shared.dirty = false;
                Some((*pane_id, shared.url.clone(), shared.title.clone()))
            })
            .collect::<Vec<_>>();
        let changed = !updates.is_empty();
        for (pane_id, url, title) in updates {
            let Some(view) = self.browser_views.get_mut(&pane_id) else {
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
        }
        changed
    }

    #[cfg(all(target_os = "macos", feature = "browser"))]
    fn flush_browser_state_updates(&mut self, cx: &mut Context<Self>) {
        let updates = self
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

        let client = Arc::clone(&self.control_client);
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
                    let Some(view) = this.browser_views.get_mut(&pane_id) else {
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
                                    this.browser_url_editor.as_mut().filter(|editor| {
                                        editor.pane_id == pane_id && editor.replace_on_type
                                    })
                                && editor.text != url
                            {
                                editor.text = url;
                                notify = true;
                            }
                        }
                        Ok(response) => {
                            if view.pending_state.is_none() {
                                view.pending_state = Some(submitted);
                            }
                            this.connection_error =
                                Some(format!("unexpected response: {response:?}"));
                            notify = true;
                        }
                        Err(error) => {
                            if view.pending_state.is_none() {
                                view.pending_state = Some(submitted);
                            }
                            this.connection_error = Some(format!("{error:#}"));
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

    #[cfg(not(all(target_os = "macos", feature = "browser")))]
    fn sync_browser_callback_state(&mut self) -> bool {
        false
    }

    #[cfg(not(all(target_os = "macos", feature = "browser")))]
    fn flush_browser_state_updates(&mut self, _cx: &mut Context<Self>) {}

    fn render_browser_toolbar(
        &self,
        pane_id: Uuid,
        url: &str,
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self
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

    #[cfg(all(target_os = "macos", feature = "browser"))]
    fn browser_views_url(&self, pane_id: Uuid) -> Option<String> {
        self.browser_views
            .get(&pane_id)
            .map(|view| view.shared.borrow().url.clone())
            .or_else(|| {
                self.pane_metadata(pane_id)
                    .and_then(|pane| match pane.kind {
                        PaneKind::Browser { url } => Some(url),
                        PaneKind::Terminal => None,
                    })
            })
    }

    #[cfg(not(all(target_os = "macos", feature = "browser")))]
    fn browser_views_url(&self, pane_id: Uuid) -> Option<String> {
        self.pane_metadata(pane_id)
            .and_then(|pane| match pane.kind {
                PaneKind::Browser { url } => Some(url),
                PaneKind::Terminal => None,
            })
    }

    #[cfg(all(target_os = "macos", feature = "browser"))]
    fn render_browser_workspace(
        &mut self,
        pane: &Pane,
        panes: Vec<Pane>,
        width: f32,
        height: f32,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PaneKind::Browser { url } = &pane.kind else {
            unreachable!("browser renderer received a terminal pane");
        };
        if let Err(error) = self.ensure_browser_view(pane.id, url, width, height) {
            self.browser_runtime_error = Some(format!("{error:#}"));
        }
        let state = self.browser_views.get(&pane.id).map(|view| {
            let state = view.shared.borrow();
            (
                state.url.clone(),
                state.loading,
                state.can_go_back,
                state.can_go_forward,
                Rc::clone(&view.pane),
            )
        });
        let (current_url, loading, can_go_back, can_go_forward) = state.as_ref().map_or_else(
            || (url.clone(), false, false, false),
            |(url, loading, back, forward, _)| (url.clone(), *loading, *back, *forward),
        );
        let toolbar = self.render_browser_toolbar(
            pane.id,
            &current_url,
            loading,
            can_go_back,
            can_go_forward,
            cx,
        );
        let content = if let Some((_, _, _, _, browser)) = state {
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    browser.set_bounds(
                        hh_cef_view::BrowserRect {
                            x: f32::from(bounds.origin.x),
                            y: f32::from(bounds.origin.y),
                            width: f32::from(bounds.size.width).max(1.0),
                            height: f32::from(bounds.size.height).max(1.0),
                        },
                        f32::from(window.bounds().size.height),
                    );
                },
            )
            .size_full()
            .into_any_element()
        } else {
            self.render_browser_placeholder(
                self.browser_runtime_error
                    .as_deref()
                    .unwrap_or("Chromium could not be initialized"),
            )
        };
        let pane_id = pane.id;
        let drop_target = self
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
                    this.dragging_pane = Some(source);
                    if let Some(placement) = split_placement_at(event.event.position, event.bounds)
                    {
                        this.drag_hover.enter(DragDestination::Split {
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
                    this.dragging_pane = Some(source);
                    if let Some(placement) = split_placement_at(event.event.position, event.bounds)
                    {
                        this.drag_hover.enter(DragDestination::Split {
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

    #[cfg(not(all(target_os = "macos", feature = "browser")))]
    fn render_browser_workspace(
        &mut self,
        pane: &Pane,
        panes: Vec<Pane>,
        _width: f32,
        _height: f32,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PaneKind::Browser { url } = &pane.kind else {
            unreachable!("browser renderer received a terminal pane");
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
                    .child(self.render_browser_placeholder(
                        "Browser tabs require the bundled macOS Chromium build",
                    )),
            )
            .into_any_element()
    }

    fn render_browser_placeholder(&self, message: &str) -> AnyElement {
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

    fn render_layout(
        &mut self,
        layout: PaneLayout,
        width: f32,
        height: f32,
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match layout {
            PaneLayout::Leaf { pane } => {
                if matches!(&pane.kind, PaneKind::Browser { .. }) {
                    self.render_browser_workspace(
                        &pane,
                        vec![pane.clone()],
                        width,
                        height,
                        show_pane_header,
                        cx,
                    )
                } else {
                    let active = pane.id;
                    self.render_terminal(vec![pane], active, show_pane_header, cx)
                }
            }
            PaneLayout::Stack { panes, active } => {
                if let Some(pane) = panes
                    .iter()
                    .find(|pane| pane.id == active)
                    .filter(|pane| matches!(pane.kind, PaneKind::Browser { .. }))
                    .cloned()
                {
                    self.render_browser_workspace(&pane, panes, width, height, show_pane_header, cx)
                } else {
                    self.render_terminal(panes, active, show_pane_header, cx)
                }
            }
            PaneLayout::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let split_id = split_control_id(&first, &second);
                let ratio = effective_split_ratio(
                    axis,
                    width,
                    height,
                    self.split_ratios.get(&split_id).copied().unwrap_or(ratio),
                );
                let vertical = axis == SplitAxis::Vertical;
                let (first_width, first_height, second_width, second_height) =
                    split_child_dimensions(axis, width, height, ratio);
                div()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .flex()
                    .when(vertical, |element| element.flex_col())
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .when(vertical, |element| element.h(relative(ratio)).w_full())
                            .when(!vertical, |element| element.w(relative(ratio)).h_full())
                            .child(self.render_layout(*first, first_width, first_height, true, cx)),
                    )
                    .child(self.render_divider(split_id, axis, cx))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .min_h(px(0.0))
                            .flex_1()
                            .child(self.render_layout(
                                *second,
                                second_width,
                                second_height,
                                true,
                                cx,
                            )),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_divider(
        &self,
        split_id: SplitControlId,
        axis: SplitAxis,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let vertical = axis == SplitAxis::Vertical;
        div()
            .id(("divider", split_element_key(split_id)))
            .flex_none()
            .when(vertical, |element| {
                element
                    .w_full()
                    .h(px(4.0))
                    .cursor(CursorStyle::ResizeUpDown)
            })
            .when(!vertical, |element| {
                element
                    .h_full()
                    .w(px(4.0))
                    .cursor(CursorStyle::ResizeLeftRight)
            })
            .bg(rgb(THEME.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.resizing = Some(ResizeDrag { split_id, axis });
                    this.focus_handle.focus(window);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn binding_label(&self, command: AppCommand) -> String {
        self.keymap
            .bindings
            .iter()
            .filter(|binding| binding.command == command)
            .map(|binding| binding.sequence.as_str())
            .collect::<Vec<_>>()
            .join("  ")
    }

    fn render_command_palette(
        &self,
        palette: &CommandPaletteState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let matches = palette_matches(&palette.query, COMMAND_PALETTE_LIMIT)
            .into_iter()
            .filter(|item| item.command != AppCommand::NewBrowserTab || browser_command_available())
            .collect::<Vec<_>>();
        let query = if palette.query.is_empty() {
            "Type a command…".to_owned()
        } else {
            palette.query.clone()
        };
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .bg(rgba(0x00000070))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(110.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.modal = Modal::None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("command-palette")
                    .w(px(620.0))
                    .h_auto()
                    .max_h(relative(0.75))
                    .overflow_y_scroll()
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(rgb(THEME.border_strong))
                    .bg(rgb(THEME.elevated))
                    .shadow_lg()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        div()
                            .h(px(48.0))
                            .px(px(15.0))
                            .border_b_1()
                            .border_color(rgb(THEME.border))
                            .flex()
                            .items_center()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(if palette.query.is_empty() {
                                rgb(THEME.dim)
                            } else {
                                rgb(THEME.foreground)
                            })
                            .child(query),
                    )
                    .children(matches.into_iter().enumerate().map(|(index, item)| {
                        let command = item.command;
                        let metadata = descriptor(command);
                        let selected = index == palette.selected;
                        div()
                            .id(("palette-command", index))
                            .h(px(44.0))
                            .px(px(13.0))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .when(selected, |element| element.bg(rgb(THEME.selection)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.execute_command(command, cx);
                                cx.stop_propagation();
                            }))
                            .child(
                                div()
                                    .w(px(210.0))
                                    .font_family(".SystemUIFont")
                                    .text_xs()
                                    .text_color(rgb(THEME.dim))
                                    .child(format!("{} · {}", metadata.category, metadata.id)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .font_family(".SystemUIFont")
                                    .text_sm()
                                    .text_color(rgb(THEME.foreground))
                                    .child(metadata.title),
                            )
                            .child(
                                div()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(rgb(THEME.muted))
                                    .child(self.binding_label(command)),
                            )
                    })),
            )
            .into_any_element()
    }
    fn render_workspace_tab_strip(
        &self,
        workspace: &Workspace,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace_id = workspace.id;
        let tab_set = workspace_tab_set(workspace, self.workspace_tab_scope);
        let scope = tab_set.scope;
        let target_tab = match scope {
            WorkspaceTabScope::Workstation => None,
            WorkspaceTabScope::Project(project_id) => Some(project_id),
        };
        let active_tab = workspace_strip_active_tab(workspace, scope, self.focused_pane);
        let tabs = tab_set
            .tabs
            .into_iter()
            .filter(|tab| !self.dismissed_workspace_tabs.contains(&tab.id))
            .filter_map(|tab| {
                let pane_id = workspace_tab_focus_target(tab, self.focused_pane)?;
                let active = active_tab == Some(tab.id);
                let standalone_pane = workspace_tab_standalone_pane(tab);
                let is_standalone = standalone_pane.is_some();
                let label = standalone_pane.map_or_else(
                    || {
                        tab.custom_title
                            .clone()
                            .unwrap_or_else(|| tab.title.clone())
                    },
                    |pane| tab_identity_presentation(pane).label,
                );
                let icon = if let Some(pane) = standalone_pane {
                    let accent = pane
                        .color
                        .unwrap_or_else(|| self.terminal_accent(pane.id))
                        .as_rgb();
                    div()
                        .flex_none()
                        .w(px(IDENTITY_MARK_SIZE))
                        .h(px(IDENTITY_MARK_SIZE))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(self.render_pane_identity_mark(
                            pane,
                            if active {
                                THEME.foreground
                            } else {
                                THEME.muted
                            },
                            accent,
                        ))
                        .into_any_element()
                } else if let Some(path) = tab
                    .custom_icon
                    .as_deref()
                    .and_then(|icon| self.custom_icon_path(icon))
                {
                    img(path)
                        .flex_none()
                        .w(px(13.0))
                        .h(px(13.0))
                        .object_fit(gpui::ObjectFit::Contain)
                        .rounded(px(2.0))
                        .into_any_element()
                } else {
                    div()
                        .flex_none()
                        .w(px(7.0))
                        .h(px(7.0))
                        .rounded_full()
                        .bg(rgb(tab.color.map_or(THEME.dim, AppearanceColor::as_rgb)))
                        .into_any_element()
                };
                let pane_count = {
                    let mut panes = Vec::new();
                    collect_terminal_tabs(&tab.layout, &mut panes);
                    panes.len()
                };
                let tab_id = tab.id;
                let close_tooltip = if is_standalone {
                    format!("Delete {label}…")
                } else {
                    format!("Remove {label} from top bar")
                };
                let background = tab.color.map_or(
                    if active {
                        THEME.elevated
                    } else {
                        THEME.surface
                    },
                    |color| composite_rgb(color.as_rgb(), THEME.surface, TAB_COLOR_ALPHA),
                );
                Some(
                    div()
                        .id(("workspace-strip-tab", element_key(tab_id)))
                        .group("workspace-strip-tab")
                        .h_full()
                        .flex_shrink()
                        .overflow_hidden()
                        .min_w(px(64.0))
                        .max_w(px(220.0))
                        .px(px(9.0))
                        .border_r_1()
                        .border_color(rgb(THEME.border))
                        .bg(rgb(background))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .when(active, |element| {
                            element.border_b_2().border_color(rgb(THEME.accent))
                        })
                        .hover(|element| element.bg(rgb(THEME.elevated)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_workspace_tab_root(workspace_id, tab_id, cx);
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                if !is_standalone {
                                    return;
                                }
                                this.open_tab_menu(pane_id, event.position, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .child(icon)
                        .child(
                            div()
                                .min_w(px(0.0))
                                .flex_1()
                                .truncate()
                                .whitespace_nowrap()
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .font_weight(if active {
                                    gpui::FontWeight::SEMIBOLD
                                } else {
                                    gpui::FontWeight::NORMAL
                                })
                                .text_color(rgb(if active {
                                    THEME.foreground
                                } else {
                                    THEME.muted
                                }))
                                .child(label),
                        )
                        .when(pane_count > 1, |element| {
                            element.child(
                                div()
                                    .flex_none()
                                    .font_family("SF Mono")
                                    .text_xs()
                                    .text_color(rgb(THEME.dim))
                                    .child(pane_count.to_string()),
                            )
                        })
                        .child(
                            div()
                                .id(("close-workspace-strip-tab", element_key(tab_id)))
                                .flex_none()
                                .w(px(16.0))
                                .h(px(16.0))
                                .rounded(px(3.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .hover(|element| {
                                    element
                                        .bg(rgb(THEME.accent_soft))
                                        .text_color(rgb(THEME.foreground))
                                })
                                .tooltip(move |_, cx| {
                                    cx.new(|_| TooltipView {
                                        text: close_tooltip.clone(),
                                    })
                                    .into()
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dismiss_workspace_tab(tab_id, cx);
                                    cx.stop_propagation();
                                }))
                                .child("×"),
                        )
                        .into_any_element(),
                )
            });
        div()
            .id(("workspace-tab-strip", element_key(workspace_id)))
            .h(px(32.0))
            .flex_none()
            .border_b_1()
            .border_color(rgb(THEME.border_strong))
            .bg(rgb(THEME.surface))
            .flex()
            .items_center()
            .child(
                div()
                    .id(("workspace-strip-scroll", element_key(workspace_id)))
                    .min_w(px(0.0))
                    .h_full()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .children(tabs),
            )
            .child(
                div()
                    .id(("new-workspace-strip-tab", element_key(workspace_id)))
                    .flex_none()
                    .w(px(31.0))
                    .h_full()
                    .border_l_1()
                    .border_color(rgb(THEME.border))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .hover(|element| {
                        element
                            .bg(rgb(THEME.elevated))
                            .text_color(rgb(THEME.foreground))
                    })
                    .tooltip(|_, cx| {
                        cx.new(|_| TooltipView {
                            text: "Add project, terminal, browser, or group".to_owned(),
                        })
                        .into()
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.modal = Modal::CreateMenu(CreateMenu {
                            position: event.position(),
                            target: CreateMenuTarget::TabStrip {
                                workspace_id,
                                target_tab,
                            },
                        });
                        cx.notify();
                    }))
                    .child("+"),
            )
            .into_any_element()
    }

    fn render_workspace(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = &self.snapshot else {
            return div()
                .size_full()
                .bg(rgb(THEME.terminal))
                .flex()
                .items_center()
                .justify_center()
                .font_family("SF Mono")
                .text_sm()
                .text_color(rgb(THEME.muted))
                .child("session service unavailable")
                .into_any_element();
        };
        let Some(workspace) = self.active_workspace_in(snapshot) else {
            return div().size_full().bg(rgb(THEME.terminal)).into_any_element();
        };
        let workspace_id = workspace.id;
        let empty_workspace_uses_ssh =
            matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. });
        let open_terminal_binding = self.binding_label(AppCommand::NewTab);
        let workspace_tab_strip = self.render_workspace_tab_strip(workspace, cx);
        // A workstation owns several top-level terminal tabs. The service
        // validates activation by pane ID, while the desktop owns the visible
        // tab selection through `focused_pane`. Rendering the first tab here
        // hid every later (including runtime-only tmux) tab even after a
        // successful sidebar click, so route the viewport to the tab that
        // contains the focused pane instead.
        let canonical_layout =
            workspace_layout_for_focused_pane(workspace, self.focused_pane).cloned();
        let standalone_root = self.focused_pane.is_some_and(|pane_id| {
            workspace
                .tabs
                .iter()
                .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                .is_some_and(|tab| workspace_tab_standalone_pane(tab).is_some())
        });
        let layout = canonical_layout.as_ref().map(|layout| {
            self.zoomed_pane
                .and_then(|pane_id| zoom_projection(layout, pane_id))
                .unwrap_or_else(|| layout.clone())
        });
        #[cfg(all(target_os = "macos", feature = "browser"))]
        let visible_browsers: HashSet<Uuid> = layout
            .as_ref()
            .map(|layout| {
                visible_panes(layout)
                    .into_iter()
                    .filter(|pane_id| {
                        find_pane(layout, *pane_id)
                            .is_some_and(|pane| matches!(pane.kind, PaneKind::Browser { .. }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let workspace_content = if let Some(layout) = layout {
            self.render_layout(
                layout,
                self.workspace_pixels.0,
                self.workspace_pixels.1,
                !standalone_root,
                cx,
            )
        } else {
            div()
                .size_full()
                .bg(rgb(THEME.terminal))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(420.0))
                        .p(px(24.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(rgb(THEME.border_strong))
                        .bg(rgb(THEME.surface))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            div()
                                .font_family("SF Mono")
                                .text_lg()
                                .text_color(rgb(THEME.foreground))
                                .child(">_"),
                        )
                        .child(
                            div()
                                .font_family(".SystemUIFont")
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(THEME.foreground))
                                .child("No terminals open"),
                        )
                        .child(
                            div()
                                .font_family(".SystemUIFont")
                                .text_sm()
                                .text_color(rgb(THEME.muted))
                                .text_center()
                                .child(if empty_workspace_uses_ssh {
                                    "Open a fresh remote terminal with this workstation's saved system OpenSSH destination."
                                } else {
                                    "This workstation is saved and ready when you want another local shell."
                                }),
                        )
                        .child(
                            div()
                                .id("open-empty-workspace-terminal")
                                .mt(px(4.0))
                                .px(px(16.0))
                                .py(px(9.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .bg(rgb(THEME.accent))
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(0xffffff))
                                .hover(|element| element.bg(rgb(THEME.ansi[4])))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_workspace_terminal(workspace_id, cx)
                                }))
                                .child("Open Terminal"),
                        )
                        .child(
                            div()
                                .font_family("SF Mono")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child(format!(
                                    "Press {open_terminal_binding} to open a terminal"
                                )),
                        ),
                )
                .into_any_element()
        };
        #[cfg(all(target_os = "macos", feature = "browser"))]
        self.sync_browser_view_presentation(&visible_browsers);
        let workspace_content = if matches!(self.modal, Modal::AppearanceSettings) {
            self.render_appearance_settings(cx)
        } else {
            workspace_content
        };
        div()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .flex_1()
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .child(workspace_tab_strip)
            .child(div().min_h(px(0.0)).flex_1().child(workspace_content))
            .into_any_element()
    }
}

impl Render for HhApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_window_geometry(window);

        // The workspace dialog has its own focus targets. A pointer click on
        // the sidebar button must not leave native text input attached to the
        // terminal behind the dialog.
        if let Some(dialog) = self.modal.workspace_creation() {
            self.workspace_input_focus[dialog.field.index()].focus(window);
        } else if self.browser_url_editor.is_some()
            || self.modal.pane_rename().is_some()
            || self.modal.workspace_rename().is_some()
            || self.modal.group_rename().is_some()
            || self.modal.dir_editor().is_some()
        {
            // Keep custom text editors on the root input route so native child
            // views cannot consume replacement typing.
            self.focus_handle.focus(window);
        }
        let modal_element = match &self.modal {
            Modal::None | Modal::AppearanceSettings | Modal::Search(_) => None,
            Modal::CommandPalette(palette) => Some(self.render_command_palette(palette, cx)),
            Modal::WorkspaceCreation(dialog) => {
                Some(self.render_workspace_creation_dialog(dialog, cx))
            }
            Modal::WorkspaceRename(editor) => Some(self.render_workspace_rename_dialog(editor, cx)),
            Modal::DirEditor(editor) => Some(self.render_dir_editor_dialog(editor, cx)),
            Modal::PaneRename(editor) => Some(self.render_rename_dialog(editor, cx)),
            Modal::GroupRename(editor) => Some(self.render_group_rename_dialog(editor, cx)),
            Modal::WorkspaceDelete(confirmation) => {
                Some(self.render_workspace_delete_dialog(confirmation, cx))
            }
            Modal::TmuxPicker(picker) => Some(self.render_tmux_session_picker(picker, cx)),
            Modal::WorkspaceDisconnect(confirmation) => {
                Some(self.render_workspace_disconnect_dialog(confirmation, cx))
            }
            Modal::Close(confirmation) => Some(self.render_close_dialog(confirmation, cx)),
            Modal::TabClose(confirmation) => Some(self.render_tab_close_dialog(confirmation, cx)),
            Modal::TabMenu(menu) => Some(self.render_tab_menu(*menu, cx)),
            Modal::WorkspaceMenu(menu) => Some(self.render_workspace_menu(*menu, cx)),
            Modal::CreateMenu(menu) => Some(self.render_create_menu(*menu, cx)),
            Modal::GroupMenu(menu) => Some(self.render_group_menu(*menu, cx)),
            Modal::WorkspaceConnectionInfo(info) => {
                Some(self.render_workspace_connection_info(info, cx))
            }
        };

        div()
            .key_context(if self.modal.command_palette().is_some() {
                "HhPalette"
            } else {
                ROOT_KEY_CONTEXT
            })
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .min_w(px(720.0))
            .min_h(px(460.0))
            .bg(rgb(THEME.window))
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx)
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.handle_resize(event, window, cx)
            }))
            .on_drag_move::<PaneDrag>(cx.listener(
                |this, event: &gpui::DragMoveEvent<PaneDrag>, _, cx| {
                    this.dragging_pane = Some(event.drag(cx).pane_id);
                    this.drag_hover.clear();
                    cx.notify();
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_resize(cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if matches!(
                        this.modal,
                        Modal::TabMenu(_)
                            | Modal::WorkspaceMenu(_)
                            | Modal::CreateMenu(_)
                            | Modal::GroupMenu(_)
                            | Modal::WorkspaceConnectionInfo(_)
                    ) {
                        this.modal = Modal::None;
                        cx.notify();
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &NewWorkspace, _, cx| {
                this.execute_command(AppCommand::NewWorkspace, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.execute_command(AppCommand::ToggleSidebar, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| {
                this.execute_command(AppCommand::NewTab, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &NewBrowserTab, _, cx| {
                this.execute_command(AppCommand::NewBrowserTab, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &TerminalZoomIn, _, cx| {
                this.execute_command(AppCommand::TerminalZoomIn, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &TerminalZoomOut, _, cx| {
                this.execute_command(AppCommand::TerminalZoomOut, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &SplitRight, _, cx| {
                this.execute_command(AppCommand::SplitRight, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &SplitDown, _, cx| {
                this.execute_command(AppCommand::SplitDown, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusLeft, _, cx| {
                this.execute_command(AppCommand::FocusLeft, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusUp, _, cx| {
                this.execute_command(AppCommand::FocusUp, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusRight, _, cx| {
                this.execute_command(AppCommand::FocusRight, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &FocusDown, _, cx| {
                this.execute_command(AppCommand::FocusDown, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ShowCommandPalette, _, cx| {
                this.execute_command(AppCommand::ShowCommandPalette, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &TogglePaneZoom, _, cx| {
                this.execute_command(AppCommand::TogglePaneZoom, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &EqualizePanes, _, cx| {
                this.execute_command(AppCommand::EqualizePanes, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ReattachPane, _, cx| {
                this.execute_command(AppCommand::ReattachPane, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &ShowNotifications, _, cx| {
                this.execute_command(AppCommand::ShowNotifications, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|_: &mut HhApp, _: &ConsumeChordPrefix, _, cx| {
                cx.stop_propagation();
            }))
            .on_action(cx.listener(HhApp::copy_terminal))
            .on_action(cx.listener(HhApp::paste_terminal))
            .on_action(cx.listener(HhApp::find_terminal))
            .on_action(cx.listener(HhApp::find_next_terminal))
            .child(
                div()
                    .absolute()
                    .w(px(1.0))
                    .h(px(1.0))
                    .child(TerminalInputElement { input: cx.entity() }),
            )
            .when(self.sidebar_resize.is_active(), |element| {
                element.child(
                    div()
                        .absolute()
                        .w(px(1.0))
                        .h(px(1.0))
                        .child(SidebarResizeCaptureElement { input: cx.entity() }),
                )
            })
            // The global navigation shares the macOS titlebar row. The rail
            // begins directly beneath it instead of rendering under traffic
            // lights or under a redundant second bar.
            .child(self.render_global_navigation(cx))
            .child(
                div()
                    .relative()
                    .min_h(px(0.0))
                    .flex_1()
                    .flex()
                    .when(self.sidebar_visible, |element| {
                        element
                            .child(self.render_sidebar(cx))
                            .child(self.render_sidebar_resize_handle(cx))
                    })
                    .child(self.render_workspace(cx)),
            )
            .when_some(modal_element, |element, modal| element.child(modal))
            .when_some(
                self.color_picker.as_ref().filter(|picker| {
                    matches!(
                        picker.target,
                        ColorTarget::DefaultTerminal | ColorTarget::DefaultWorkspace
                    )
                }),
                |element, picker| element.child(self.render_color_picker(picker, cx)),
            )
    }
}

impl EntityInputHandler for HhApp {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if let Some(dialog) = self
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            let byte_range = editor.range_from_utf16(&range);
            actual_range.replace(editor.range_to_utf16(&byte_range));
            return Some(editor.text[byte_range].to_owned());
        }
        actual_range.replace(0..self.ime_preedit.encode_utf16().count());
        Some(self.ime_preedit.clone())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(dialog) = self
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            return Some(UTF16Selection {
                range: editor.range_to_utf16(&editor.selected_range),
                reversed: editor.selection_reversed,
            });
        }
        let end = self.ime_preedit.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        if let Some(dialog) = self
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let editor = dialog.active_editor();
            return editor
                .marked_range
                .as_ref()
                .map(|range| editor.range_to_utf16(range));
        }
        (!self.ime_preedit.is_empty()).then(|| 0..self.ime_preedit.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self
            .modal
            .workspace_creation_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.active_editor_mut().marked_range = None;
            cx.notify();
            return;
        }
        self.ime_preedit.clear();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self
            .modal
            .workspace_creation_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.replace_text(range.as_ref(), text, false, None);
            cx.notify();
            return;
        }
        self.ime_preedit.clear();
        self.commit_text(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self
            .modal
            .workspace_creation_mut()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            dialog.replace_text(range.as_ref(), text, true, selected_range.as_ref());
            cx.notify();
            return;
        }
        text.clone_into(&mut self.ime_preedit);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if let Some(dialog) = self
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let index = dialog.field.index();
            let (Some(line), Some(input_bounds)) = (
                self.workspace_input_layouts[index].as_ref(),
                self.workspace_input_bounds[index],
            ) else {
                return None;
            };
            let byte_range = dialog.active_editor().range_from_utf16(&range);
            return Some(Bounds::from_corners(
                point(
                    input_bounds.left() + line.x_for_index(byte_range.start),
                    input_bounds.top(),
                ),
                point(
                    input_bounds.left() + line.x_for_index(byte_range.end),
                    input_bounds.bottom(),
                ),
            ));
        }
        let line_height = self
            .focused_pane
            .map_or(self.terminal_font.metrics.line_height, |pane_id| {
                self.terminal_metrics(pane_id).line_height
            });
        Some(Bounds::new(
            bounds.bottom_left(),
            size(px(1.0), px(line_height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if let Some(dialog) = self
            .modal
            .workspace_creation()
            .filter(|dialog| dialog.step == WorkspaceCreationStep::Details)
        {
            let index = dialog.field.index();
            let (Some(line), Some(bounds)) = (
                self.workspace_input_layouts[index].as_ref(),
                self.workspace_input_bounds[index],
            ) else {
                return None;
            };
            let byte_index = line.closest_index_for_x(point.x - bounds.left());
            return Some(dialog.active_editor().offset_to_utf16(byte_index));
        }
        Some(0)
    }
}

struct WorkspaceTextInputElement {
    input: Entity<HhApp>,
    field: WorkspaceCreationField,
    placeholder: &'static str,
}

struct WorkspaceTextPrepaintState {
    line: ShapedLine,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    text_bounds: Bounds<Pixels>,
    active: bool,
}

impl IntoElement for WorkspaceTextInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WorkspaceTextInputElement {
    type RequestLayoutState = ();
    type PrepaintState = WorkspaceTextPrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let app = self.input.read(cx);
        let dialog = app.modal.workspace_creation();
        let active = dialog.is_some_and(|dialog| {
            dialog.step == WorkspaceCreationStep::Details && dialog.field == self.field
        });
        let editor = dialog.map(|dialog| match self.field {
            WorkspaceCreationField::Name => &dialog.name,
            WorkspaceCreationField::Destination => &dialog.destination,
        });
        let content = editor.map_or("", |editor| editor.text.as_str());
        let display_text = if content.is_empty() {
            self.placeholder
        } else {
            content
        };
        let style = window.text_style();
        let base_run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: if content.is_empty() {
                rgb(THEME.dim).into()
            } else {
                style.color
            },
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs =
            if let Some(marked_range) = editor.and_then(|editor| editor.marked_range.as_ref()) {
                vec![
                    TextRun {
                        len: marked_range.start,
                        ..base_run.clone()
                    },
                    TextRun {
                        len: marked_range.len(),
                        underline: Some(UnderlineStyle {
                            color: Some(base_run.color),
                            thickness: px(1.0),
                            wavy: false,
                        }),
                        ..base_run.clone()
                    },
                    TextRun {
                        len: content.len().saturating_sub(marked_range.end),
                        ..base_run
                    },
                ]
                .into_iter()
                .filter(|run| run.len > 0)
                .collect()
            } else {
                vec![base_run]
            };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line =
            window
                .text_system()
                .shape_line(display_text.to_owned().into(), font_size, &runs, None);
        let selected_range = editor.map_or(0..0, |editor| editor.selected_range.clone());
        let cursor_offset = editor.map_or(0, DialogTextEditor::cursor_offset);
        let cursor_x = line.x_for_index(cursor_offset);
        let scroll_x = if active {
            (cursor_x - (bounds.size.width - px(2.0))).max(px(0.0))
        } else {
            px(0.0)
        };
        let text_bounds = Bounds::new(point(bounds.left() - scroll_x, bounds.top()), bounds.size);
        let (selection, cursor) = if active && !selected_range.is_empty() {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            text_bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            text_bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x62adff40),
                )),
                None,
            )
        } else if active {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(
                            text_bounds.left() + line.x_for_index(cursor_offset),
                            bounds.top(),
                        ),
                        size(px(1.5), bounds.bottom() - bounds.top()),
                    ),
                    rgb(THEME.accent),
                )),
            )
        } else {
            (None, None)
        };

        WorkspaceTextPrepaintState {
            line,
            cursor,
            selection,
            text_bounds,
            active,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if state.active {
            let focus_handle =
                self.input.read(cx).workspace_input_focus[self.field.index()].clone();
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
        if state
            .line
            .paint(state.text_bounds.origin, window.line_height(), window, cx)
            .is_err()
        {
            return;
        }
        if let Some(selection) = state.selection.take() {
            window.paint_quad(selection);
        }
        if let Some(cursor) = state.cursor.take() {
            window.paint_quad(cursor);
        }
        let line = state.line.clone();
        let field_index = self.field.index();
        self.input.update(cx, |app, _| {
            app.workspace_input_layouts[field_index] = Some(line);
            app.workspace_input_bounds[field_index] = Some(state.text_bounds);
        });
    }
}

struct TerminalInputElement {
    input: Entity<HhApp>,
}

impl IntoElement for TerminalInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalInputElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let app = self.input.read(cx);
        if app.modal.workspace_creation().is_none() {
            window.handle_input(
                &app.focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
    }
}

/// Registers window-level listeners while the sidebar divider owns an active
/// pointer gesture. GPUI's normal element listeners are hover-scoped, while a
/// resize capture must continue to receive drag and release events outside the
/// divider (and even outside the window bounds when the platform delivers them).
struct SidebarResizeCaptureElement {
    input: Entity<HhApp>,
}

impl IntoElement for SidebarResizeCaptureElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SidebarResizeCaptureElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (window.request_layout(Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let input = self.input.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                input.update(cx, |this, cx| this.handle_resize(event, window, cx));
                cx.stop_propagation();
            }
        });

        let input = self.input.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                input.update(cx, |this, cx| this.finish_resize(cx));
                cx.stop_propagation();
            }
        });
    }
}

#[cfg(target_os = "macos")]
fn set_macos_dock_badge(label: Option<&str>) {
    hh_macos_icon::set_dock_badge(label);
}

#[cfg(not(target_os = "macos"))]
fn set_macos_dock_badge(_: Option<&str>) {}

/// One hit surface per terminal row keeps pointer semantics exact without
/// forcing GPUI/Taffy to lay out an element for every visible grid cell.
struct TerminalPointerElement {
    input: Entity<HhApp>,
    pane_id: Uuid,
    row: u16,
    columns: u16,
    cell_width: f32,
}

impl IntoElement for TerminalPointerElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalPointerElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, HitboxBehavior::BlockMouse)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        let pointer_hitbox = hitbox.clone();
        let input = self.input.clone();
        let pane_id = self.pane_id;
        let row = self.row;
        let columns = self.columns;
        let cell_width = self.cell_width;
        let hitbox = pointer_hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.begin_terminal_pointer(pane_id, point, event, window, cx);
                });
            }
        });

        let input = self.input.clone();
        let hitbox = pointer_hitbox.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.move_terminal_pointer(pane_id, point, event, cx);
                });
            }
        });

        let input = self.input.clone();
        let hitbox = pointer_hitbox.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.end_terminal_pointer(pane_id, point, event, cx);
                });
            }
        });

        let input = self.input.clone();
        let hitbox = pointer_hitbox;
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                let point = terminal_point_at(event.position, bounds, row, columns, cell_width);
                input.update(cx, |this, cx| {
                    this.scroll_terminal(pane_id, point, event, cx);
                });
            }
        });
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
    std::env::var(DEVELOPMENT_BUILD_ENV).as_deref() == Ok("1")
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

/// Keep the banner available to the native renderer even while a Dev bundle is
/// rebuilt in place. The same user-owned artwork remains packaged as a bundle
/// resource; this stable in-process source prevents an asynchronous file-load
/// miss from leaving the rail header blank after a relaunch.
fn workstation_banner_artwork() -> BannerArtwork {
    static BANNER: LazyLock<BannerArtwork> = LazyLock::new(|| BannerArtwork {
        image: Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../assets/harnessharlot-banner.png").to_vec(),
        )),
        width: BUNDLED_BANNER_PIXEL_WIDTH,
        height: BUNDLED_BANNER_PIXEL_HEIGHT,
    });
    BANNER.clone()
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

#[cfg(all(target_os = "macos", feature = "browser"))]
static BROWSER_COMMAND_AVAILABLE: LazyLock<bool> = LazyLock::new(hh_cef_view::preflight);

#[cfg(all(target_os = "macos", feature = "browser"))]
fn browser_command_available() -> bool {
    *BROWSER_COMMAND_AVAILABLE
}

#[cfg(not(all(target_os = "macos", feature = "browser")))]
fn browser_command_available() -> bool {
    false
}

#[cfg(all(target_os = "macos", feature = "browser"))]
fn native_nsview(window: &Window) -> Option<*mut c_void> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(handle) => Some(handle.ns_view.as_ptr()),
        _ => None,
    }
}

#[cfg(all(target_os = "macos", feature = "browser"))]
fn prepare_cef_process() {
    if !hh_cef_view::preflight() {
        return;
    }
    if let Some(exit_code) = hh_cef_view::early_process_split() {
        std::process::exit(exit_code);
    }
    hh_cef_view::install_nsapp_protocol();
}

#[cfg(not(all(target_os = "macos", feature = "browser")))]
fn prepare_cef_process() {}

fn main() {
    prepare_cef_process();
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
    use super::*;

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
    #[test]
    fn project_icon_detection_uses_documented_root_and_name_precedence() {
        let directory = std::env::temp_dir().join(format!("hh-project-icon-{}", Uuid::new_v4()));
        std::fs::create_dir_all(directory.join("public")).unwrap();
        std::fs::write(directory.join("public/icon.png"), b"public icon").unwrap();
        std::fs::write(directory.join("logo.png"), b"root logo").unwrap();
        assert_eq!(
            detected_project_icon(&directory),
            Some(directory.join("logo.png"))
        );
        std::fs::write(directory.join("icon.png"), b"root icon").unwrap();
        assert_eq!(
            detected_project_icon(&directory),
            Some(directory.join("icon.png"))
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn remote_completion_splits_paths_and_extends_common_prefix() {
        assert_eq!(
            remote_completion_parts("/srv/pro"),
            ("/srv/".to_owned(), "pro".to_owned())
        );
        assert_eq!(
            remote_completion_parts("/srv/"),
            ("/srv/".to_owned(), String::new())
        );
        assert_eq!(
            longest_common_prefix(&["project-a".to_owned(), "project-b".to_owned()]),
            "project-"
        );
    }

    #[test]
    fn browser_url_key_text_falls_back_to_printable_physical_keys() {
        assert_eq!(
            browser_key_text("a", None, false, false).as_deref(),
            Some("a")
        );
        assert_eq!(
            browser_key_text("a", None, true, false).as_deref(),
            Some("A")
        );
        assert_eq!(
            browser_key_text("space", None, false, false).as_deref(),
            Some(" ")
        );
        assert_eq!(browser_key_text("left", None, false, false), None);
        assert_eq!(browser_key_text("a", None, false, true), None);
    }
}
