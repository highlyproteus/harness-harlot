use super::*;

pub(super) fn terminal_point_at(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    row: u16,
    columns: u16,
    cell_width: f32,
) -> TerminalPoint {
    let relative_x = f32::from(position.x - bounds.origin.x).max(0.0);
    let column = if columns == 0 || cell_width <= f32::EPSILON {
        0
    } else {
        (relative_x / cell_width).floor() as u16
    };
    TerminalPoint {
        row,
        column: column.min(columns.saturating_sub(1)),
    }
}

pub(super) fn next_terminal_poll_delay_ms(
    current: u64,
    state_changed: bool,
    deep_idle: bool,
) -> u64 {
    if state_changed {
        ACTIVE_TERMINAL_POLL_MS
    } else if deep_idle {
        DEEP_IDLE_POLL_MS
    } else {
        current.saturating_mul(2).min(IDLE_TERMINAL_POLL_MS)
    }
}

pub(super) fn pane_update_requires_repaint(
    snapshot_delivered: bool,
    screens_delivered: usize,
) -> bool {
    snapshot_delivered || screens_delivered > 0
}

/// The focused pane streams every poll. Other on-screen panes are paced so a
/// four-way split cannot multiply one pane's payload by four every 33 ms.
pub(super) fn paced_subscriptions(
    now: Instant,
    on_screen: &[Uuid],
    focused: Option<Uuid>,
    last_delivery: &HashMap<Uuid, Instant>,
    interval: Duration,
) -> Vec<Uuid> {
    on_screen
        .iter()
        .copied()
        .filter(|pane_id| {
            Some(*pane_id) == focused
                || last_delivery
                    .get(pane_id)
                    .is_none_or(|last| now.saturating_duration_since(*last) >= interval)
        })
        .collect()
}

pub(super) fn terminal_mouse_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

pub(super) fn terminal_modifiers(modifiers: gpui::Modifiers) -> TerminalModifiers {
    TerminalModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
    }
}

pub(super) fn selection_span(
    selection: TerminalSelection,
    row: usize,
    columns: u16,
) -> Option<(u16, u16)> {
    let row = u16::try_from(row).ok()?;
    if row < selection.start.row || row > selection.end.row || columns == 0 {
        return None;
    }
    let start = if selection.is_block || row == selection.start.row {
        selection.start.column.min(columns - 1)
    } else {
        0
    };
    let end = if selection.is_block || row == selection.end.row {
        selection.end.column.min(columns - 1)
    } else {
        columns - 1
    };
    (end >= start).then_some((start, end - start + 1))
}

pub(super) fn prepare_paste(text: &str, bracketed: bool) -> Result<Vec<u8>, &'static str> {
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r");
    let sanitized = normalized.replace(['\0', '\u{1b}'], "");
    let wrapper_size = if bracketed { 12 } else { 0 };
    if sanitized.len().saturating_add(wrapper_size) > MAX_PASTE_BYTES {
        return Err("paste rejected: clipboard text exceeds 64 KiB");
    }
    if bracketed {
        let mut bytes = Vec::with_capacity(sanitized.len() + wrapper_size);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(sanitized.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        Ok(bytes)
    } else {
        Ok(sanitized.into_bytes())
    }
}

pub(super) fn visible_panes(layout: &PaneLayout) -> Vec<Uuid> {
    match layout {
        PaneLayout::Leaf { pane } => vec![pane.id],
        PaneLayout::Stack { active, .. } => vec![*active],
        PaneLayout::Split { first, second, .. } => {
            let mut panes = visible_panes(first);
            panes.extend(visible_panes(second));
            panes
        }
    }
}

pub(super) fn split_target_for_drag(source: Uuid, panes: &[Pane], active: Uuid) -> Option<Uuid> {
    let pane_ids = panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
    split_target_for_drag_ids(source, &pane_ids, active)
}

pub(super) fn split_target_for_drag_ids(
    source: Uuid,
    pane_ids: &[Uuid],
    active: Uuid,
) -> Option<Uuid> {
    if source == active {
        pane_ids
            .iter()
            .copied()
            .find(|pane| *pane != source)
            .or_else(|| (pane_ids.len() == 1).then_some(active))
    } else {
        Some(active)
    }
}

pub(super) fn split_placement_at(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
) -> Option<DropPlacement> {
    if !bounds.contains(&position) {
        return None;
    }
    let x = f32::from(position.x - bounds.origin.x);
    let y = f32::from(position.y - bounds.origin.y);
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if y < PANE_HEADER_HEIGHT || width <= 0.0 || height <= PANE_HEADER_HEIGHT {
        return None;
    }
    if x <= width * 0.25 {
        Some(DropPlacement::Left)
    } else if x >= width * 0.75 {
        Some(DropPlacement::Right)
    } else if y - PANE_HEADER_HEIGHT <= (height - PANE_HEADER_HEIGHT) * 0.5 {
        Some(DropPlacement::Top)
    } else {
        Some(DropPlacement::Bottom)
    }
}

pub(super) fn history_label(label: &'static str) -> AnyElement {
    div()
        .w(px(76.0))
        .font_family(".SystemUIFont")
        .text_xs()
        .text_color(rgb(THEME.muted))
        .child(label)
        .into_any_element()
}

pub(super) fn history_scope_key(scope: HistoryClearScope) -> usize {
    match scope {
        HistoryClearScope::Terminal { .. } => 0,
        HistoryClearScope::Workspace { .. } => 1,
        HistoryClearScope::All => 2,
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{}.{} GiB", bytes / GIB, (bytes % GIB) * 10 / GIB)
    } else {
        format!("{}.{} MiB", bytes / MIB, (bytes % MIB) * 10 / MIB)
    }
}

pub(super) fn format_history_date(milliseconds: u64) -> String {
    let days = i64::try_from(milliseconds / 1_000 / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

pub(super) fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

pub(super) fn history_warning_text(
    warning: Option<HistoryWarning>,
    dropped_bytes: u64,
) -> Option<String> {
    match warning {
    Some(HistoryWarning::ApproachingCapacity) => Some(
        "Archive is nearing its quota. Increase the limit or clear selected history before it fills."
            .to_owned(),
    ),
    Some(HistoryWarning::PausedAtCapacity) => Some(format!(
        "Archive is full and paused; the terminal is still live. {} could not be archived. Increase the quota or clear selected history.",
        format_bytes(dropped_bytes)
    )),
    Some(HistoryWarning::QueueOverflow) => Some(format!(
        "The storage queue could not keep up; {} is marked as an archive gap. Terminal input and output continued normally.",
        format_bytes(dropped_bytes)
    )),
    Some(HistoryWarning::CorruptChunk) => Some(
        "A local archive chunk failed integrity checks. It is shown as a gap; other chunks remain available."
            .to_owned(),
    ),
    None => None,
}
}

pub(super) fn plain_history_line(text: &str) -> TerminalLine {
    TerminalLine {
        runs: if text.is_empty() {
            Vec::new()
        } else {
            vec![TerminalRun {
                text: text.to_owned(),
                columns: text.chars().fold(0_u16, |columns, character| {
                    columns.saturating_add(
                        u16::try_from(if character == '\t' {
                            1
                        } else {
                            character.width().unwrap_or(0)
                        })
                        .unwrap_or(u16::MAX),
                    )
                }),
                foreground: TerminalColor::DefaultForeground,
                background: TerminalColor::DefaultBackground,
                attributes: TerminalAttributes::default(),
            }]
        },
    }
}

pub(super) fn terminal_run_display_text(run: &TerminalRun, _start_column: u16) -> String {
    // The terminal model already represents every occupied grid cell,
    // including the cells skipped by a tab. Render its tab cell as one
    // blank cell instead of asking GPUI to apply proportional tab stops.
    run.text.replace('\t', " ")
}

pub(super) fn find_pane(layout: &PaneLayout, pane_id: Uuid) -> Option<&Pane> {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => Some(pane),
        PaneLayout::Leaf { .. } => None,
        PaneLayout::Stack { panes, .. } => panes.iter().find(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            find_pane(first, pane_id).or_else(|| find_pane(second, pane_id))
        }
    }
}

pub(super) fn collect_terminal_tabs<'a>(layout: &'a PaneLayout, panes: &mut Vec<&'a Pane>) {
    match layout {
        PaneLayout::Leaf { pane } => panes.push(pane),
        PaneLayout::Stack { panes: stacked, .. } => panes.extend(stacked),
        PaneLayout::Split { first, second, .. } => {
            collect_terminal_tabs(first, panes);
            collect_terminal_tabs(second, panes);
        }
    }
}

pub(super) fn workspace_terminal_tabs(workspace: &Workspace) -> Vec<&Pane> {
    let mut panes = Vec::new();
    for tab in &workspace.tabs {
        collect_terminal_tabs(&tab.layout, &mut panes);
    }
    panes
}

/// One sidebar entry per tab. `group_label` is `Some` exactly when the tab
/// must render as a group: it holds several terminals, or the user named it.
pub(super) struct WorkstationTabEntry<'a> {
    pub(super) tab_id: Uuid,
    pub(super) group_label: Option<String>,
    pub(super) panes: Vec<&'a Pane>,
}

pub(super) fn workspace_tab_entries(workspace: &Workspace) -> Vec<WorkstationTabEntry<'_>> {
    workspace
        .tabs
        .iter()
        .map(|tab| {
            let mut panes = Vec::new();
            collect_terminal_tabs(&tab.layout, &mut panes);
            let group_label = (panes.len() >= 2 || tab.custom_title.is_some()).then(|| {
                tab.custom_title
                    .clone()
                    .unwrap_or_else(|| tab.title.clone())
            });
            WorkstationTabEntry {
                tab_id: tab.id,
                group_label,
                panes,
            }
        })
        .collect()
}

/// Visible panes across every tab of one workstation, in tab order.
///
/// Focus bookkeeping must reason about the whole workstation: a runtime-only
/// tmux tab is never the first tab, so scoping this to `tabs.first()` would
/// treat a perfectly live focused pane as gone and snap the viewport back to
/// the initial terminal on the next poll.
pub(super) fn workspace_visible_panes(workspace: &Workspace) -> Vec<Uuid> {
    workspace
        .tabs
        .iter()
        .flat_map(|tab| visible_panes(&tab.layout))
        .collect()
}

/// Outcome of reconciling the focused pane against a fresh snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FocusResync {
    /// The focused pane is still on screen somewhere in the workstation.
    Keep,
    /// The focused pane is gone; fall back to the workstation's first pane.
    Switch(Uuid),
    /// The workstation has no visible pane left.
    Clear,
}

pub(super) fn focus_resync_for(visible: &[Uuid], focused: Option<Uuid>) -> FocusResync {
    if focused.is_some_and(|pane_id| visible.contains(&pane_id)) {
        return FocusResync::Keep;
    }
    visible
        .first()
        .copied()
        .map_or(FocusResync::Clear, FocusResync::Switch)
}

pub(super) fn workspace_layout_for_focused_pane(
    workspace: &Workspace,
    focused_pane: Option<Uuid>,
) -> Option<&PaneLayout> {
    focused_pane
        .and_then(|pane_id| {
            workspace
                .tabs
                .iter()
                .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                .map(|tab| &tab.layout)
        })
        .or_else(|| workspace.tabs.first().map(|tab| &tab.layout))
}

pub(super) fn terminal_tab_count_label(count: usize) -> String {
    format!("{count} terminal{}", if count == 1 { "" } else { "s" })
}

pub(super) fn tab_identity_presentation(pane: &Pane) -> TabIdentityPresentation {
    let detection_detail = match pane.identity.source {
        nah_protocol::TerminalIdentitySource::UserRename => "Custom terminal name",
        nah_protocol::TerminalIdentitySource::UserProfile => "User-selected local profile",
        nah_protocol::TerminalIdentitySource::TerminalTitle => {
            "Detected from a bounded terminal-title signal; terminal content is not inspected"
        }
        nah_protocol::TerminalIdentitySource::Command => {
            "Detected from a bounded local child-process name; terminal content is not inspected"
        }
        nah_protocol::TerminalIdentitySource::Fallback => "Ordinary terminal",
    };
    let definition = agent_icon_definition(pane.identity.profile);
    let asset_detail = if pane.custom_icon.is_some() {
        "A reusable user-uploaded image is selected"
    } else if definition.asset.is_some() {
        "Bundled official artwork is shown unchanged for referential identification only; no affiliation or endorsement is implied"
    } else if pane.identity.profile == TerminalProfile::GitHubCopilot {
        "The official CLI package exposes no standalone icon asset, so Not a Harness uses the neutral terminal glyph"
    } else {
        "Not a Harness uses the neutral terminal glyph"
    };
    TabIdentityPresentation {
        label: pane.title.clone(),
        profile: pane.identity.profile,
        detail: format!(
            "{} — {detection_detail}. {asset_detail}.",
            definition.accessible_name
        ),
    }
}

pub(super) fn terminal_tab_secondary_label(pane: &Pane) -> Option<&str> {
    pane.custom_title.is_none().then_some(pane.shell.as_str())
}

pub(super) const IDENTITY_MARK_SIZE: f32 = 22.0;
pub(super) const OFFICIAL_IDENTITY_ICON_SIZE: f32 = 20.0;
pub(super) const FALLBACK_IDENTITY_ICON_SIZE: f32 = 14.0;
const FALLBACK_IDENTITY_FRAME_SIZE: f32 = 18.0;

pub(super) fn terminal_profile_icon_is_framed(profile: TerminalProfile) -> bool {
    agent_icon_definition(profile).asset.is_none()
}

pub(super) fn terminal_profile_icon_size(profile: TerminalProfile) -> f32 {
    if terminal_profile_icon_is_framed(profile) {
        FALLBACK_IDENTITY_ICON_SIZE
    } else {
        OFFICIAL_IDENTITY_ICON_SIZE
    }
}

pub(super) fn render_terminal_profile_mark(
    profile: TerminalProfile,
    fallback_color: u32,
    fallback_border_color: u32,
) -> AnyElement {
    let icon =
        render_terminal_profile_icon(profile, fallback_color, terminal_profile_icon_size(profile));
    if terminal_profile_icon_is_framed(profile) {
        div()
            .w(px(FALLBACK_IDENTITY_FRAME_SIZE))
            .h(px(FALLBACK_IDENTITY_FRAME_SIZE))
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(fallback_border_color))
            .flex()
            .items_center()
            .justify_center()
            .child(icon)
            .into_any_element()
    } else {
        icon
    }
}

pub(super) fn render_sidebar_toggle_icon(sidebar_visible: bool) -> AnyElement {
    div()
        .relative()
        .w(px(15.0))
        .h(px(13.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(rgb(THEME.muted))
        .child(
            div()
                .absolute()
                .left(px(4.0))
                .top(px(0.0))
                .w(px(1.0))
                .h_full()
                .bg(rgb(THEME.muted)),
        )
        .child(
            div()
                .absolute()
                .left(px(if sidebar_visible { 1.5 } else { 7.0 }))
                .top(px(3.0))
                .w(px(if sidebar_visible { 1.5 } else { 4.0 }))
                .h(px(5.0))
                .rounded(px(1.0))
                .bg(rgb(THEME.muted)),
        )
        .into_any_element()
}

pub(super) fn render_terminal_profile_icon(
    profile: TerminalProfile,
    fallback_color: u32,
    icon_size: f32,
) -> AnyElement {
    let definition = agent_icon_definition(profile);
    let icon = match definition.asset {
        Some(asset) if asset.format == AgentIconFormat::Svg => svg()
            .path(asset.path)
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(rgb(fallback_color))
            .into_any_element(),
        Some(asset) => img(asset.path)
            .w(px(icon_size))
            .h(px(icon_size))
            .object_fit(gpui::ObjectFit::Contain)
            .into_any_element(),
        None => div()
            .font_family("SF Mono")
            .text_size(px(7.5))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(rgb(fallback_color))
            .child(profile.fallback_glyph())
            .into_any_element(),
    };
    div()
        .w(px(icon_size))
        .h(px(icon_size))
        .flex()
        .items_center()
        .justify_center()
        .child(icon)
        .into_any_element()
}

pub(super) fn resolved_terminal_accent(
    snapshot: &SessionSnapshot,
    pane_id: Uuid,
) -> AppearanceColor {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find_map(|tab| find_pane(&tab.layout, pane_id))
        .and_then(|pane| pane.color)
        .unwrap_or(snapshot.appearance.default_terminal_accent)
}

pub(super) fn resolved_workspace_color(
    snapshot: &SessionSnapshot,
    workspace_id: Uuid,
) -> AppearanceColor {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == workspace_id)
        .and_then(|workspace| workspace.color)
        .unwrap_or(snapshot.appearance.default_workspace_color)
}

pub(super) fn workspace_is_selectable(workspace: &Workspace) -> bool {
    workspace.tabs.is_empty()
        || !matches!(
            workspace.connection,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            }
        )
}

pub(super) fn stable_representative_pane(layout: &PaneLayout) -> Uuid {
    match layout {
        PaneLayout::Leaf { pane } => pane.id,
        PaneLayout::Stack { panes, active } => panes.first().map_or(*active, |pane| pane.id),
        PaneLayout::Split { first, .. } => stable_representative_pane(first),
    }
}

pub(super) fn split_control_id(first: &PaneLayout, second: &PaneLayout) -> SplitControlId {
    SplitControlId {
        first: stable_representative_pane(first),
        second: stable_representative_pane(second),
    }
}

pub(super) fn zoom_projection(layout: &PaneLayout, pane_id: Uuid) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf { pane } => (pane.id == pane_id).then(|| layout.clone()),
        PaneLayout::Stack { panes, .. } => panes
            .iter()
            .any(|pane| pane.id == pane_id)
            .then(|| layout.clone()),
        PaneLayout::Split { first, second, .. } => {
            zoom_projection(first, pane_id).or_else(|| zoom_projection(second, pane_id))
        }
    }
}

pub(super) fn apply_layout_control_mutation(
    layout: &PaneLayout,
    ratios: &mut HashMap<SplitControlId, f32>,
    mutation: LayoutControlMutation,
) -> usize {
    match layout {
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => 0,
        PaneLayout::Split { first, second, .. } => {
            match mutation {
                LayoutControlMutation::Equalize => {
                    ratios.insert(split_control_id(first, second), 0.5);
                }
            }
            1 + apply_layout_control_mutation(first, ratios, mutation)
                + apply_layout_control_mutation(second, ratios, mutation)
        }
    }
}

pub(super) fn default_sidebar_width() -> f32 {
    default_sidebar_width_for(development_build())
}

pub(super) const fn default_sidebar_width_for(development: bool) -> f32 {
    if development {
        DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH
    } else {
        DEFAULT_SIDEBAR_WIDTH
    }
}

/// Restore only the short-lived 104 px Dev migration introduced by the
/// compact-rail experiment. Any other persisted width is user-resized data.
pub(super) fn migrated_sidebar_width(stored_width: Option<f32>) -> f32 {
    migrated_sidebar_width_for(stored_width, development_build())
}

pub(super) fn migrated_sidebar_width_for(stored_width: Option<f32>, development: bool) -> f32 {
    if development && stored_width.is_some_and(|width| (width - 104.0).abs() < f32::EPSILON) {
        DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH
    } else {
        stored_width.unwrap_or_else(|| default_sidebar_width_for(development))
    }
}

pub(super) fn constrained_sidebar_width(preferred_width: f32, window_width: f32) -> f32 {
    let preferred_width = if preferred_width.is_finite() {
        preferred_width
    } else {
        default_sidebar_width()
    };
    let maximum_for_window =
        (window_width - MIN_TERMINAL_AREA_WIDTH).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    preferred_width.clamp(MIN_SIDEBAR_WIDTH, maximum_for_window)
}

pub(super) fn sidebar_width_for_visibility(
    preferred_width: f32,
    window_width: f32,
    visible: bool,
) -> f32 {
    if visible {
        constrained_sidebar_width(preferred_width, window_width)
    } else {
        0.0
    }
}

pub(super) fn workspace_pixel_size(
    window_width: f32,
    window_height: f32,
    sidebar_width: f32,
) -> (f32, f32) {
    (
        (window_width - sidebar_width).max(1.0),
        (window_height - APP_CHROME_HEIGHT).max(1.0),
    )
}

pub(super) const fn rgba_with_alpha(color: u32, alpha: u8) -> u32 {
    (color << 8) | alpha as u32
}

pub(super) fn composite_rgb(foreground: u32, background: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let channel = |shift| {
        let foreground = (foreground >> shift) & 0xff_u32;
        let background = (background >> shift) & 0xff_u32;
        (foreground * alpha + background * inverse + 127_u32) / 255_u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

pub(super) fn readable_text_color(background: u32) -> u32 {
    let red = (background >> 16) & 0xff;
    let green = (background >> 8) & 0xff;
    let blue = background & 0xff;
    if red * 299 + green * 587 + blue * 114 > 150_000 {
        0x111318
    } else {
        0xffffff
    }
}

pub(super) fn parse_hex_color(value: &str) -> Option<AppearanceColor> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = u32::from_str_radix(value, 16).ok()?;
    Some(AppearanceColor::new(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    ))
}

pub(super) fn effective_split_ratio(axis: SplitAxis, width: f32, height: f32, ratio: f32) -> f32 {
    let extent = match axis {
        SplitAxis::Horizontal => width,
        SplitAxis::Vertical => height,
    }
    .max(1.0);
    let minimum = match axis {
        SplitAxis::Horizontal => MIN_PANE_WIDTH,
        SplitAxis::Vertical => MIN_PANE_HEIGHT,
    };
    if extent < minimum * 2.0 + SPLIT_DIVIDER_SIZE {
        return 0.5;
    }
    let low = minimum / extent;
    let high = (extent - SPLIT_DIVIDER_SIZE - minimum) / extent;
    ratio.clamp(low, high)
}

pub(super) fn split_child_dimensions(
    axis: SplitAxis,
    width: f32,
    height: f32,
    ratio: f32,
) -> (f32, f32, f32, f32) {
    match axis {
        SplitAxis::Horizontal => {
            let first_width = (width * ratio).floor().max(1.0);
            let second_width = (width - first_width - SPLIT_DIVIDER_SIZE).max(1.0);
            (first_width, height, second_width, height)
        }
        SplitAxis::Vertical => {
            let first_height = (height * ratio).floor().max(1.0);
            let second_height = (height - first_height - SPLIT_DIVIDER_SIZE).max(1.0);
            (width, first_height, width, second_height)
        }
    }
}

pub(super) fn find_split_rect(
    layout: &PaneLayout,
    target_split_id: SplitControlId,
    rect: PixelRect,
    ratios: &HashMap<SplitControlId, f32>,
) -> Option<PixelRect> {
    let PaneLayout::Split {
        axis,
        ratio,
        first,
        second,
    } = layout
    else {
        return None;
    };
    let split_id = split_control_id(first, second);
    if split_id == target_split_id {
        return Some(rect);
    }
    let ratio = effective_split_ratio(
        *axis,
        rect.width,
        rect.height,
        ratios.get(&split_id).copied().unwrap_or(*ratio),
    );
    let (first_width, first_height, second_width, second_height) =
        split_child_dimensions(*axis, rect.width, rect.height, ratio);
    let first_rect = PixelRect {
        width: first_width,
        height: first_height,
        ..rect
    };
    let second_rect = match axis {
        SplitAxis::Horizontal => PixelRect {
            x: rect.x + first_width + SPLIT_DIVIDER_SIZE,
            y: rect.y,
            width: second_width,
            height: second_height,
        },
        SplitAxis::Vertical => PixelRect {
            x: rect.x,
            y: rect.y + first_height + SPLIT_DIVIDER_SIZE,
            width: second_width,
            height: second_height,
        },
    };
    find_split_rect(first, target_split_id, first_rect, ratios)
        .or_else(|| find_split_rect(second, target_split_id, second_rect, ratios))
}

pub(super) fn collect_pane_sizes(
    layout: &PaneLayout,
    width: f32,
    height: f32,
    metrics: typography::TerminalCellMetrics,
    ratios: &HashMap<SplitControlId, f32>,
    output: &mut Vec<(Uuid, u16, u16)>,
) {
    match layout {
        PaneLayout::Leaf { pane } => {
            let (columns, rows) = terminal_grid_for_pane(width, height, metrics);
            output.push((pane.id, columns, rows));
        }
        PaneLayout::Stack { active, .. } => {
            let (columns, rows) = terminal_grid_for_pane(width, height, metrics);
            output.push((*active, columns, rows));
        }
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = effective_split_ratio(
                *axis,
                width,
                height,
                ratios
                    .get(&split_control_id(first, second))
                    .copied()
                    .unwrap_or(*ratio),
            );
            let (first_width, first_height, second_width, second_height) =
                split_child_dimensions(*axis, width, height, ratio);
            collect_pane_sizes(first, first_width, first_height, metrics, ratios, output);
            collect_pane_sizes(second, second_width, second_height, metrics, ratios, output);
        }
    }
}

pub(super) fn terminal_input_bytes(
    key: &str,
    key_char: Option<&str>,
    control: bool,
    alt: bool,
    platform: bool,
) -> Option<Vec<u8>> {
    // Command/Super is an application modifier, not a PTY modifier. Unmatched
    // platform shortcuts remain available to the OS instead of becoming text.
    if platform {
        return None;
    }
    if control && key.len() == 1 {
        return key
            .as_bytes()
            .first()
            .map(|byte| vec![byte.to_ascii_lowercase() & 0x1f]);
    }
    let mut bytes = match key {
        "enter" => vec![b'\r'],
        "backspace" => vec![0x7f],
        "tab" => vec![b'\t'],
        "escape" => vec![0x1b],
        "left" => b"\x1b[D".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        _ => key_char?.as_bytes().to_vec(),
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

pub(super) fn terminal_grid_for_pane(
    pane_width: f32,
    pane_height: f32,
    metrics: typography::TerminalCellMetrics,
) -> (u16, u16) {
    let content_width =
        (pane_width - TERMINAL_HORIZONTAL_PADDING - TERMINAL_FOCUS_BORDER_WIDTH).max(1.0);
    let content_height = (pane_height - PANE_HEADER_HEIGHT - TERMINAL_VERTICAL_PADDING).max(1.0);
    (
        metrics.columns_for_width(content_width),
        metrics.rows_for_height(content_height),
    )
}

pub(super) fn element_key(id: Uuid) -> u64 {
    let (high, low) = id.as_u64_pair();
    high ^ low
}

pub(super) fn split_element_key(id: SplitControlId) -> u64 {
    element_key(id.first).rotate_left(17) ^ element_key(id.second)
}

pub(super) fn gpui_binding(binding: &ResolvedBinding) -> KeyBinding {
    match binding.command {
        AppCommand::NewWorkspace => {
            KeyBinding::new(&binding.sequence, NewWorkspace, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ToggleSidebar => {
            KeyBinding::new(&binding.sequence, ToggleSidebar, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::NewTab => KeyBinding::new(&binding.sequence, NewTab, Some(ROOT_KEY_CONTEXT)),
        AppCommand::SplitRight => {
            KeyBinding::new(&binding.sequence, SplitRight, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::SplitDown => {
            KeyBinding::new(&binding.sequence, SplitDown, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::FocusLeft => {
            KeyBinding::new(&binding.sequence, FocusLeft, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::FocusRight => {
            KeyBinding::new(&binding.sequence, FocusRight, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::FocusUp => KeyBinding::new(&binding.sequence, FocusUp, Some(ROOT_KEY_CONTEXT)),
        AppCommand::FocusDown => {
            KeyBinding::new(&binding.sequence, FocusDown, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ShowCommandPalette => KeyBinding::new(
            &binding.sequence,
            ShowCommandPalette,
            Some(ROOT_KEY_CONTEXT),
        ),
        AppCommand::TogglePaneZoom => {
            KeyBinding::new(&binding.sequence, TogglePaneZoom, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::EqualizePanes => {
            KeyBinding::new(&binding.sequence, EqualizePanes, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ReattachPane => {
            KeyBinding::new(&binding.sequence, ReattachPane, Some(ROOT_KEY_CONTEXT))
        }
    }
}
pub(super) fn product_name(development_build: bool) -> &'static str {
    if development_build {
        DEVELOPMENT_PRODUCT_NAME
    } else {
        STABLE_PRODUCT_NAME
    }
}

pub(super) fn workstation_banner_header_height(sidebar_content_width: f32) -> f32 {
    sidebar_content_width.max(0.0) / WORKSTATION_BANNER_ASPECT_RATIO
}

pub(super) fn append_rename_text(value: &mut String, replace_on_type: &mut bool, text: &str) {
    if *replace_on_type {
        value.clear();
    }
    let remaining = 80_usize.saturating_sub(value.chars().count());
    value.extend(
        text.chars()
            .filter(|character| !character.is_control())
            .take(remaining),
    );
    *replace_on_type = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_offline_ssh_workspace_is_selectable_only_for_its_reopen_affordance() {
        let mut snapshot = SessionSnapshot::seeded();
        let workspace = &mut snapshot.workspaces[0];
        workspace.connection = WorkspaceConnection::SystemSsh {
            destination: "tailnet-host".to_owned(),
            status: WorkspaceConnectionStatus::Offline,
        };
        assert!(!workspace_is_selectable(workspace));

        workspace.tabs.clear();

        assert!(workspace_is_selectable(workspace));
    }
    #[test]
    fn native_tab_identity_label_and_icon_registry_smoke_test() {
        let cases = [
            (TerminalProfile::Terminal, false),
            (TerminalProfile::Hermes, true),
            (TerminalProfile::Codex, true),
            (TerminalProfile::Claude, true),
            (TerminalProfile::Droid, true),
            (TerminalProfile::KiloCode, true),
            (TerminalProfile::Cursor, true),
            (TerminalProfile::OpenCode, true),
            (TerminalProfile::Aider, true),
            (TerminalProfile::GitHubCopilot, false),
            (TerminalProfile::Gemini, true),
        ];
        for (profile, has_official_asset) in cases {
            let label = profile.display_name();
            let pane = Pane {
                id: Uuid::new_v4(),
                title: label.to_owned(),
                shell: "zsh".to_owned(),
                color: None,
                identity: nah_protocol::TerminalIdentity {
                    profile,
                    source: if profile == TerminalProfile::Terminal {
                        nah_protocol::TerminalIdentitySource::Fallback
                    } else {
                        nah_protocol::TerminalIdentitySource::Command
                    },
                },
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };

            let presentation = tab_identity_presentation(&pane);
            assert_eq!(presentation.label, label);
            assert_eq!(presentation.profile, profile);
            assert!(presentation.detail.contains(label));
            assert_eq!(
                agent_icon_definition(profile).asset.is_some(),
                has_official_asset
            );
            assert_eq!(
                terminal_profile_icon_is_framed(profile),
                !has_official_asset
            );
            let expected_size = if has_official_asset {
                OFFICIAL_IDENTITY_ICON_SIZE
            } else {
                FALLBACK_IDENTITY_ICON_SIZE
            };
            assert!((terminal_profile_icon_size(profile) - expected_size).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn renamed_tab_hides_shell_metadata_that_would_displace_its_name() {
        let mut pane = Pane {
            id: Uuid::new_v4(),
            title: "Release terminal".to_owned(),
            shell: "ssh release@long-production-host.example.com".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        assert_eq!(
            terminal_tab_secondary_label(&pane),
            Some("ssh release@long-production-host.example.com")
        );

        pane.custom_title = Some(pane.title.clone());

        assert_eq!(terminal_tab_secondary_label(&pane), None);
        assert_eq!(tab_identity_presentation(&pane).label, "Release terminal");
    }
    #[test]
    fn workspace_rail_lists_every_terminal_tab_across_stacks_and_splits() {
        let make_pane = |id: u128, title: &str, profile: TerminalProfile| Pane {
            id: Uuid::from_u128(id),
            title: title.to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity {
                profile,
                source: nah_protocol::TerminalIdentitySource::Command,
            },
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let codex = make_pane(1, "Codex review", TerminalProfile::Codex);
        let droid = make_pane(2, "Droid build", TerminalProfile::Droid);
        let terminal = make_pane(3, "Logs", TerminalProfile::Terminal);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs[0].layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Stack {
                panes: vec![codex.clone(), droid.clone()],
                active: droid.id,
            }),
            second: Box::new(PaneLayout::Leaf {
                pane: terminal.clone(),
            }),
        };

        let tabs = workspace_terminal_tabs(&workspace);

        assert_eq!(
            tabs.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            vec![codex.id, droid.id, terminal.id]
        );
        assert_eq!(
            tabs.iter()
                .map(|pane| tab_identity_presentation(pane).profile)
                .collect::<Vec<_>>(),
            vec![
                TerminalProfile::Codex,
                TerminalProfile::Droid,
                TerminalProfile::Terminal
            ]
        );
        assert_eq!(terminal_tab_count_label(tabs.len()), "3 terminals");
    }

    #[test]
    fn workspace_tab_projection_preserves_tab_order_and_group_identity() {
        let make_pane = |id: u128| Pane {
            id: Uuid::from_u128(id),
            title: format!("Terminal {id}"),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            nah_protocol::Tab {
                id: Uuid::from_u128(10),
                title: "Single".to_owned(),
                custom_title: None,
                layout: PaneLayout::Leaf { pane: make_pane(1) },
            },
            nah_protocol::Tab {
                id: Uuid::from_u128(20),
                title: "Named".to_owned(),
                custom_title: Some("Group 1".to_owned()),
                layout: PaneLayout::Leaf { pane: make_pane(2) },
            },
            nah_protocol::Tab {
                id: Uuid::from_u128(30),
                title: "Stacked".to_owned(),
                custom_title: None,
                layout: PaneLayout::Stack {
                    panes: vec![make_pane(3), make_pane(4)],
                    active: Uuid::from_u128(3),
                },
            },
            nah_protocol::Tab {
                id: Uuid::from_u128(40),
                title: "Split".to_owned(),
                custom_title: None,
                layout: PaneLayout::Split {
                    axis: SplitAxis::Horizontal,
                    ratio: 0.5,
                    first: Box::new(PaneLayout::Leaf { pane: make_pane(5) }),
                    second: Box::new(PaneLayout::Leaf { pane: make_pane(6) }),
                },
            },
        ];

        let entries = workspace_tab_entries(&workspace);

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.group_label.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("Group 1"), Some("Stacked"), Some("Split")]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.panes.len())
                .collect::<Vec<_>>(),
            vec![1, 1, 2, 2]
        );
        assert_eq!(
            entries.iter().map(|entry| entry.tab_id).collect::<Vec<_>>(),
            vec![
                Uuid::from_u128(10),
                Uuid::from_u128(20),
                Uuid::from_u128(30),
                Uuid::from_u128(40)
            ]
        );
    }
    #[test]
    fn runtime_tmux_tab_panes_stay_visible_to_focus_bookkeeping() {
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        let initial = visible_panes(&workspace.tabs[0].layout)[0];
        let tmux_pane = Uuid::from_u128(0x77);
        workspace.tabs.push(nah_protocol::Tab {
            id: Uuid::from_u128(0x88),
            title: "buzz".to_owned(),
            custom_title: None,
            layout: PaneLayout::Leaf {
                pane: Pane {
                    id: tmux_pane,
                    title: "tmux buzz".to_owned(),
                    shell: "tmux".to_owned(),
                    color: None,
                    identity: nah_protocol::TerminalIdentity::default(),
                    custom_title: None,
                    profile_override: None,
                    custom_icon: None,
                },
            },
        });

        // The attached tmux tab is never first, so first-tab-only bookkeeping
        // treated its pane as gone and snapped focus back to the SSH shell on
        // the very next poll, leaving the tmux tab unrenderable.
        let visible = workspace_visible_panes(&workspace);
        assert_eq!(visible, vec![initial, tmux_pane]);
        assert_eq!(
            focus_resync_for(&visible, Some(tmux_pane)),
            FocusResync::Keep
        );
        assert_eq!(
            workspace_layout_for_focused_pane(&workspace, Some(tmux_pane)),
            Some(&workspace.tabs[1].layout)
        );

        // A pane that really vanished still falls back to the first tab, and an
        // empty workstation clears focus outright.
        assert_eq!(
            focus_resync_for(&visible, Some(Uuid::from_u128(0x99))),
            FocusResync::Switch(initial)
        );
        assert_eq!(
            focus_resync_for(&visible, None),
            FocusResync::Switch(initial)
        );
        assert_eq!(focus_resync_for(&[], Some(tmux_pane)), FocusResync::Clear);
    }

    #[test]
    fn workspace_rail_empty_state_and_tab_count_labels_are_explicit() {
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs.clear();

        assert!(workspace_terminal_tabs(&workspace).is_empty());
        assert_eq!(terminal_tab_count_label(0), "0 terminals");
        assert_eq!(terminal_tab_count_label(1), "1 terminal");
    }
    #[test]
    fn workstation_rows_start_collapsed_but_can_expand_after_creation() {
        let workstation = SessionSnapshot::seeded().workspaces.remove(0);
        let mut expanded_workstations: HashSet<Uuid> = HashSet::new();

        assert!(!expanded_workstations.contains(&workstation.id));
        assert_eq!(
            terminal_tab_count_label(workspace_terminal_tabs(&workstation).len()),
            "1 terminal"
        );

        assert!(expanded_workstations.insert(workstation.id));
        assert!(expanded_workstations.contains(&workstation.id));
    }
    #[test]
    fn appearance_color_precedence_keeps_terminal_and_workspace_scopes_independent() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = visible_panes(&snapshot.workspaces[0].tabs[0].layout)[0];
        let workspace_id = snapshot.workspaces[0].id;
        let terminal_default = AppearanceColor::new(0x95, 0xcc, 0x7f);
        let workspace_default = AppearanceColor::new(0xc9, 0x90, 0xe5);
        let terminal_override = AppearanceColor::new(0xef, 0x71, 0x7a);
        let workspace_override = AppearanceColor::new(0xe4, 0xbd, 0x72);
        snapshot.appearance.default_terminal_accent = terminal_default;
        snapshot.appearance.default_workspace_color = workspace_default;

        assert_eq!(
            resolved_terminal_accent(&snapshot, pane_id),
            terminal_default
        );
        assert_eq!(
            resolved_workspace_color(&snapshot, workspace_id),
            workspace_default
        );

        let PaneLayout::Leaf { pane } = &mut snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected leaf");
        };
        pane.color = Some(terminal_override);
        snapshot.workspaces[0].color = Some(workspace_override);

        assert_eq!(
            resolved_terminal_accent(&snapshot, pane_id),
            terminal_override
        );
        assert_eq!(
            resolved_workspace_color(&snapshot, workspace_id),
            workspace_override
        );
        snapshot.workspaces[0].color = None;
        assert_eq!(
            resolved_terminal_accent(&snapshot, pane_id),
            terminal_override
        );
        assert_eq!(
            resolved_workspace_color(&snapshot, workspace_id),
            workspace_default
        );
    }
    #[test]
    fn color_picker_accepts_exact_hex_and_rejects_partial_or_non_hex_input() {
        assert_eq!(
            parse_hex_color("#67C8C6"),
            Some(AppearanceColor::new(0x67, 0xc8, 0xc6))
        );
        assert_eq!(
            parse_hex_color("62adff"),
            Some(AppearanceColor::HARBOR_BLUE)
        );
        assert_eq!(parse_hex_color("FFF"), None);
        assert_eq!(parse_hex_color("GGADFF"), None);
    }

    #[test]
    fn alpha_color_helpers_encode_and_composite_exact_channels() {
        assert_eq!(rgba_with_alpha(0x3b424f, 0xd0), 0x3b424fd0);
        assert_eq!(composite_rgb(0xffffff, 0x000000, 0x80), 0x808080);
        assert_eq!(composite_rgb(0x3b424f, 0x15171c, 0xff), 0x3b424f);
    }
    #[test]
    fn pointer_local_split_zones_exclude_the_tab_strip_and_cover_each_half() {
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(100.0)),
        };

        assert_eq!(split_placement_at(point(px(50.0), px(10.0)), bounds), None);
        assert_eq!(
            split_placement_at(point(px(10.0), px(50.0)), bounds),
            Some(DropPlacement::Left)
        );
        assert_eq!(
            split_placement_at(point(px(90.0), px(50.0)), bounds),
            Some(DropPlacement::Right)
        );
        assert_eq!(
            split_placement_at(point(px(50.0), px(40.0)), bounds),
            Some(DropPlacement::Top)
        );
        assert_eq!(
            split_placement_at(point(px(50.0), px(90.0)), bounds),
            Some(DropPlacement::Bottom)
        );
        assert_eq!(split_placement_at(point(px(101.0), px(50.0)), bounds), None);
    }
    #[test]
    fn multi_column_terminal_rows_keep_spaces_and_wide_cells_on_one_grid() {
        let modeled_cells = TerminalRun {
            text: "A\t  B".to_owned(),
            columns: 5,
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            attributes: TerminalAttributes::default(),
        };

        assert_eq!(terminal_run_display_text(&modeled_cells, 0), "A   B");
        assert_eq!(modeled_cells.columns, 5);
    }
    #[test]
    fn one_row_hit_surface_maps_pointer_positions_to_terminal_cells() {
        let bounds = Bounds {
            origin: point(px(100.0), px(40.0)),
            size: size(px(80.0), px(18.0)),
        };

        assert_eq!(
            terminal_point_at(point(px(100.0), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 0 }
        );
        assert_eq!(
            terminal_point_at(point(px(139.9), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 4 }
        );
        assert_eq!(
            terminal_point_at(point(px(190.0), px(49.0)), bounds, 7, 10, 8.0),
            TerminalPoint { row: 7, column: 9 }
        );
    }
    #[test]
    fn terminal_polling_is_fast_while_output_changes_and_backs_off_when_idle() {
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, true, false),
            ACTIVE_TERMINAL_POLL_MS
        );
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, true, true),
            ACTIVE_TERMINAL_POLL_MS
        );
        assert_eq!(
            next_terminal_poll_delay_ms(ACTIVE_TERMINAL_POLL_MS, false, false),
            ACTIVE_TERMINAL_POLL_MS * 2
        );
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, false, false),
            IDLE_TERMINAL_POLL_MS
        );
        assert_eq!(
            next_terminal_poll_delay_ms(IDLE_TERMINAL_POLL_MS, false, true),
            DEEP_IDLE_POLL_MS
        );
    }
    #[test]
    fn on_screen_panes_stream_with_the_focused_pane_always_and_siblings_paced() {
        let now = Instant::now();
        let focused = Uuid::from_u128(1);
        let sibling = Uuid::from_u128(2);
        let fresh = Uuid::from_u128(3);
        let on_screen = [focused, sibling];
        let recent = HashMap::from([(focused, now), (sibling, now)]);

        assert_eq!(
            paced_subscriptions(
                now,
                &on_screen,
                Some(focused),
                &recent,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused],
            "a sibling delivered this instant waits for its pacing interval"
        );

        let stale = HashMap::from([
            (focused, now),
            (
                sibling,
                now.checked_sub(Duration::from_millis(200)).unwrap(),
            ),
        ]);
        assert_eq!(
            paced_subscriptions(
                now,
                &on_screen,
                Some(focused),
                &stale,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused, sibling]
        );

        assert_eq!(
            paced_subscriptions(
                now,
                &[focused, fresh],
                Some(focused),
                &recent,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused, fresh],
            "a pane never delivered before is always subscribed"
        );

        let untouched_for_an_hour = HashMap::from([
            (focused, now),
            (sibling, now.checked_sub(Duration::from_hours(1)).unwrap()),
        ]);
        assert_eq!(
            paced_subscriptions(
                now,
                &on_screen,
                Some(focused),
                &untouched_for_an_hour,
                SECONDARY_PANE_INTERVAL
            ),
            vec![focused, sibling],
            "a visible pane never cools: subscription follows what is on screen, not attention"
        );

        assert!(
            paced_subscriptions(now, &[], Some(focused), &recent, SECONDARY_PANE_INTERVAL)
                .is_empty(),
            "nothing on screen streams nothing"
        );
    }
    #[test]
    fn revision_metadata_alone_does_not_repaint_inactive_panes() {
        assert!(!pane_update_requires_repaint(false, 0));
        assert!(pane_update_requires_repaint(false, 1));
        assert!(pane_update_requires_repaint(true, 0));
    }
    #[test]
    fn pane_geometry_tracks_narrow_medium_and_wide_windows_without_fixed_columns() {
        let pane = Pane {
            id: Uuid::from_u128(10),
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let layout = PaneLayout::Leaf { pane };
        let metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let ratios = HashMap::new();

        let dimensions = [(720.0, 460.0), (1280.0, 820.0), (1800.0, 1000.0)]
            .into_iter()
            .map(|(window_width, window_height)| {
                let workspace =
                    workspace_pixel_size(window_width, window_height, DEFAULT_SIDEBAR_WIDTH);
                let mut sizes = Vec::new();
                collect_pane_sizes(
                    &layout,
                    workspace.0,
                    workspace.1,
                    metrics,
                    &ratios,
                    &mut sizes,
                );
                sizes[0]
            })
            .collect::<Vec<_>>();

        assert_eq!(dimensions[0], (Uuid::from_u128(10), 69, 20));
        assert_eq!(dimensions[1], (Uuid::from_u128(10), 139, 39));
        assert_eq!(dimensions[2], (Uuid::from_u128(10), 204, 48));
        assert!(
            dimensions
                .windows(2)
                .all(|pair| { pair[0].1 < pair[1].1 && pair[0].2 < pair[1].2 })
        );
    }
    #[test]
    fn sidebar_width_is_bounded_without_forgetting_a_wider_preference() {
        assert!((constrained_sidebar_width(80.0, 1280.0) - MIN_SIDEBAR_WIDTH).abs() < 0.0001);
        assert!((constrained_sidebar_width(900.0, 1280.0) - 420.0).abs() < 0.0001);

        let preferred = 390.0;
        let compact = constrained_sidebar_width(preferred, 640.0);
        assert!((compact - 320.0).abs() < 0.0001);
        assert!((workspace_pixel_size(640.0, 460.0, compact).0 - 320.0).abs() < 0.0001);
        assert!((constrained_sidebar_width(preferred, 1280.0) - preferred).abs() < 0.0001);
    }
    #[test]
    fn development_sidebar_restores_the_normal_width_after_the_compact_experiment() {
        assert!(
            (default_sidebar_width_for(true) - DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH).abs()
                < f32::EPSILON
        );
        assert!(
            (migrated_sidebar_width_for(Some(104.0), true) - DEVELOPMENT_DEFAULT_SIDEBAR_WIDTH)
                .abs()
                < f32::EPSILON
        );
        assert!((migrated_sidebar_width_for(Some(356.0), true) - 356.0).abs() < f32::EPSILON);
        assert!(
            (migrated_sidebar_width_for(None, false) - DEFAULT_SIDEBAR_WIDTH).abs() < f32::EPSILON
        );
    }
    #[test]
    fn workstation_banner_header_preserves_the_artwork_aspect_at_normal_and_wide_rails() {
        for sidebar_content_width in [136.0, 217.0, 412.0] {
            let height = workstation_banner_header_height(sidebar_content_width);
            assert!(
                (height * WORKSTATION_BANNER_ASPECT_RATIO - sidebar_content_width).abs() < 0.0001
            );
        }
    }
    #[test]
    fn terminal_rename_accepts_replacement_text_after_the_original_is_cleared() {
        let mut value = "Terminal 1".to_owned();
        let mut replace_on_type = true;

        append_rename_text(&mut value, &mut replace_on_type, "Build shell");

        assert_eq!(value, "Build shell");
        assert!(!replace_on_type);

        append_rename_text(&mut value, &mut replace_on_type, "\n");
        assert_eq!(value, "Build shell");
    }
    #[test]
    fn hidden_sidebar_gives_the_workspace_the_full_window_width() {
        let visible = sidebar_width_for_visibility(260.0, 1280.0, true);
        let hidden = sidebar_width_for_visibility(260.0, 1280.0, false);

        assert!((visible - 260.0).abs() < f32::EPSILON);
        assert!(hidden.abs() < f32::EPSILON);
        assert!((workspace_pixel_size(1280.0, 820.0, hidden).0 - 1280.0).abs() < f32::EPSILON);
    }
    #[test]
    fn widest_sidebar_still_leaves_two_constrained_split_panes_in_the_minimum_window() {
        let sidebar = constrained_sidebar_width(MAX_SIDEBAR_WIDTH, 720.0);
        let workspace = workspace_pixel_size(720.0, 460.0, sidebar);
        let ratio = effective_split_ratio(SplitAxis::Horizontal, workspace.0, workspace.1, 0.95);
        let (first_width, _, second_width, _) =
            split_child_dimensions(SplitAxis::Horizontal, workspace.0, workspace.1, ratio);

        assert!((sidebar - 400.0).abs() < 0.0001);
        assert!((workspace.0 - MIN_TERMINAL_AREA_WIDTH).abs() < 0.0001);
        assert!(first_width >= MIN_PANE_WIDTH);
        assert!(second_width >= MIN_PANE_WIDTH);
    }
    #[test]
    fn split_geometry_accounts_for_the_divider_and_each_panes_chrome() {
        let first = Pane {
            id: Uuid::from_u128(21),
            title: "Terminal 1".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let second = Pane {
            id: Uuid::from_u128(22),
            title: "Terminal 2".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf {
                pane: first.clone(),
            }),
            second: Box::new(PaneLayout::Leaf { pane: second }),
        };
        let metrics = typography::TerminalCellMetrics {
            font_size: 13.5,
            cell_width: 8.0,
            ascent: 10.0,
            descent: 3.0,
            baseline: 13.0,
            line_height: 19.0,
        };
        let workspace = workspace_pixel_size(1280.0, 820.0, DEFAULT_SIDEBAR_WIDTH);
        let mut sizes = Vec::new();
        collect_pane_sizes(
            &layout,
            workspace.0,
            workspace.1,
            metrics,
            &HashMap::new(),
            &mut sizes,
        );

        assert_eq!(
            sizes,
            vec![(first.id, 68, 39), (Uuid::from_u128(22), 68, 39)]
        );
        let used_pixel_width = 568.0 + SPLIT_DIVIDER_SIZE + 564.0;
        assert!((used_pixel_width - workspace.0).abs() < 0.0001);
    }
    #[test]
    fn split_ratio_respects_practical_pane_constraints_at_each_window_size() {
        let narrow = effective_split_ratio(SplitAxis::Horizontal, 530.0, 422.0, 0.05);
        let wide = effective_split_ratio(SplitAxis::Horizontal, 1610.0, 962.0, 0.05);
        assert!((narrow - (MIN_PANE_WIDTH / 530.0)).abs() < 0.0001);
        assert!((wide - (MIN_PANE_WIDTH / 1610.0)).abs() < 0.0001);

        let too_short = effective_split_ratio(SplitAxis::Vertical, 530.0, 150.0, 0.9);
        assert!((too_short - 0.5).abs() < 0.0001);
    }
    #[test]
    fn terminal_input_encodes_unmatched_keys_once_with_control_and_alt_semantics() {
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, false),
            Some(vec![b'x'])
        );
        assert_eq!(
            terminal_input_bytes("c", Some("c"), true, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, true, false),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            terminal_input_bytes("up", None, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            terminal_input_bytes("x", Some("x"), false, false, true),
            None
        );
    }
    #[test]
    fn bracketed_paste_normalizes_newlines_and_cannot_inject_an_early_end_marker() {
        let bytes = prepare_paste("one\n\x1b[201~two\r\n", true).unwrap();
        assert_eq!(bytes, b"\x1b[200~one\r[201~two\r\x1b[201~");
        assert_eq!(
            bytes
                .windows(b"\x1b[201~".len())
                .filter(|window| *window == b"\x1b[201~")
                .count(),
            1
        );
    }
    #[test]
    fn focused_workspace_tab_layout_is_rendered_instead_of_the_first_tab() {
        let pane = |id, title: &str| Pane {
            id: Uuid::from_u128(id),
            title: title.to_owned(),
            shell: "tmux".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let first = pane(1, "SSH");
        let tmux = pane(2, "tmux $2");
        let workspace = Workspace {
            id: Uuid::nil(),
            title: "Remote".to_owned(),
            color: None,
            pinned: false,
            pin_order: 0,
            order: 0,
            active_terminal_count: 2,
            connection: WorkspaceConnection::Local,
            tabs: vec![
                nah_protocol::Tab {
                    id: Uuid::from_u128(10),
                    title: "SSH".to_owned(),
                    custom_title: None,
                    layout: PaneLayout::Leaf {
                        pane: first.clone(),
                    },
                },
                nah_protocol::Tab {
                    id: Uuid::from_u128(20),
                    title: "tmux".to_owned(),
                    custom_title: None,
                    layout: PaneLayout::Leaf { pane: tmux.clone() },
                },
            ],
        };

        assert_eq!(
            workspace_layout_for_focused_pane(&workspace, Some(tmux.id)),
            Some(&PaneLayout::Leaf { pane: tmux })
        );
        assert_eq!(
            workspace_layout_for_focused_pane(&workspace, Some(Uuid::from_u128(99))),
            Some(&PaneLayout::Leaf { pane: first })
        );
    }
    #[test]
    fn zoom_is_a_projection_that_does_not_mutate_canonical_layout() {
        let first = Pane {
            id: Uuid::from_u128(101),
            title: "one".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let second = Pane {
            id: Uuid::from_u128(102),
            title: "two".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.3,
            first: Box::new(PaneLayout::Leaf {
                pane: first.clone(),
            }),
            second: Box::new(PaneLayout::Stack {
                panes: vec![second.clone()],
                active: second.id,
            }),
        };
        let before = layout.clone();

        assert_eq!(
            zoom_projection(&layout, second.id),
            Some(PaneLayout::Stack {
                panes: vec![second.clone()],
                active: second.id
            })
        );
        assert_eq!(layout, before);
        assert_eq!(zoom_projection(&layout, Uuid::from_u128(999)), None);
    }
    #[test]
    fn equalize_is_a_controlled_mutation_over_all_current_split_identities() {
        let pane = |id| Pane {
            id: Uuid::from_u128(id),
            title: format!("pane {id}"),
            shell: "zsh".to_owned(),
            color: None,
            identity: nah_protocol::TerminalIdentity::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let nested = PaneLayout::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.8,
            first: Box::new(PaneLayout::Leaf { pane: pane(2) }),
            second: Box::new(PaneLayout::Leaf { pane: pane(3) }),
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.2,
            first: Box::new(PaneLayout::Leaf { pane: pane(1) }),
            second: Box::new(nested),
        };
        let mut ratios = HashMap::from([
            (
                SplitControlId {
                    first: Uuid::from_u128(1),
                    second: Uuid::from_u128(2),
                },
                0.1,
            ),
            (
                SplitControlId {
                    first: Uuid::from_u128(2),
                    second: Uuid::from_u128(3),
                },
                0.9,
            ),
        ]);

        let changed =
            apply_layout_control_mutation(&layout, &mut ratios, LayoutControlMutation::Equalize);

        assert_eq!(changed, 2);
        assert!(
            (ratios[&SplitControlId {
                first: Uuid::from_u128(1),
                second: Uuid::from_u128(2)
            }] - 0.5)
                .abs()
                < f32::EPSILON
        );
        assert!(
            (ratios[&SplitControlId {
                first: Uuid::from_u128(2),
                second: Uuid::from_u128(3)
            }] - 0.5)
                .abs()
                < f32::EPSILON
        );
    }
    #[test]
    fn oversized_paste_is_rejected_before_it_reaches_the_protocol() {
        let text = "x".repeat(MAX_PASTE_BYTES + 1);
        assert_eq!(
            prepare_paste(&text, false),
            Err("paste rejected: clipboard text exceeds 64 KiB")
        );
    }
    #[test]
    fn selection_highlight_spans_exact_grid_cells_across_rows() {
        let selection = TerminalSelection {
            start: TerminalPoint { row: 1, column: 3 },
            end: TerminalPoint { row: 2, column: 4 },
            is_block: false,
        };
        assert_eq!(selection_span(selection, 0, 10), None);
        assert_eq!(selection_span(selection, 1, 10), Some((3, 7)));
        assert_eq!(selection_span(selection, 2, 10), Some((0, 5)));
    }
}
