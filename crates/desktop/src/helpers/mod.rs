//! Stateless presentation helpers, grouped by concern.
mod bindings;
mod dnd;
mod geometry;
mod history_ui;
mod icons;
mod layout;
mod polling;
mod tabs;
mod terminal_io;

pub(crate) use bindings::{
    abbreviate_home, append_rename_text, element_key, gpui_binding, product_name, split_element_key,
};
pub(crate) use dnd::{
    HeaderDropZone, click_suppression_active, header_drop_zone, split_placement_at,
    split_target_for_drag, split_target_for_drag_ids,
};
pub(crate) use geometry::{
    banner_fit_size, collect_pane_sizes, composite_rgb, constrained_sidebar_width,
    default_sidebar_width, effective_split_ratio, find_split_rect, hsv_to_rgb,
    migrated_sidebar_width, parse_hex_color, readable_text_color, rgb_to_hsv, rgba_with_alpha,
    sidebar_width_for_visibility, split_child_dimensions, workspace_pixel_size,
    workstation_banner_header_height,
};
pub(crate) use history_ui::{
    LiveScrollTarget, format_bytes, format_history_date, history_label, history_scope_key,
    history_warning_text, live_scroll_target, wheel_delta_lines,
};
pub(crate) use icons::{
    IDENTITY_MARK_SIZE, render_bell_icon, render_sidebar_toggle_icon, render_terminal_profile_icon,
    render_terminal_profile_mark, resolved_terminal_accent, resolved_workspace_color,
    tab_identity_presentation, workspace_is_selectable,
};
pub(crate) use layout::{
    apply_layout_control_mutation, collect_terminal_tabs, find_pane, inactive_stack_contains,
    split_control_id, visible_panes, workspace_layout_for_focused_pane, workspace_terminal_tabs,
    workspace_visible_panes, zoom_projection,
};
pub(crate) use polling::{
    next_terminal_poll_delay_ms, paced_subscriptions, pane_update_requires_repaint,
    terminal_poll_wake_requested,
};
pub(crate) use tabs::{
    FocusResync, SidebarSection, WorkspaceTabScope, WorkstationTabEntry, focus_resync_for,
    partition_workstation_entries, terminal_tab_count_label, terminal_tab_secondary_label,
    workspace_scope_for_tab, workspace_strip_active_tab, workspace_tab_click_target,
    workspace_tab_entries, workspace_tab_focus_target, workspace_tab_set,
    workspace_tab_standalone_pane,
};
pub(crate) use terminal_io::{
    plain_history_line, prepare_paste, selection_span, terminal_grid_for_pane,
    terminal_input_bytes, terminal_modifiers, terminal_mouse_button, terminal_point_at,
    terminal_run_display_text, url_at_column,
};
