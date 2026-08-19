use gpui::{AnyElement, IntoElement, ParentElement, Styled, StyledImage, div, img, px, rgb, svg};
use hh_protocol::{
    AppearanceColor, Pane, SessionSnapshot, TerminalProfile, Workspace, WorkspaceConnection,
    WorkspaceConnectionStatus,
};
use uuid::Uuid;

use crate::THEME;
use crate::agent_icons::{AgentIconFormat, agent_icon_definition};
use crate::helpers::layout::find_pane;
use crate::view_models::TabIdentityPresentation;

pub(crate) fn tab_identity_presentation(pane: &Pane) -> TabIdentityPresentation {
    if pane.kind.is_browser() {
        return TabIdentityPresentation {
            label: pane.title.clone(),
            profile: TerminalProfile::Terminal,
            detail: "Chromium browser tab".to_owned(),
        };
    }
    let detection_detail = match pane.identity.source {
        hh_protocol::TerminalIdentitySource::UserRename => "Custom terminal name",
        hh_protocol::TerminalIdentitySource::UserProfile => "User-selected local profile",
        hh_protocol::TerminalIdentitySource::TerminalTitle => {
            "Detected from a bounded terminal-title signal; terminal content is not inspected"
        }
        hh_protocol::TerminalIdentitySource::Command => {
            "Detected from a bounded local child-process name; terminal content is not inspected"
        }
        hh_protocol::TerminalIdentitySource::Fallback => "Ordinary terminal",
    };
    let definition = agent_icon_definition(pane.identity.profile);
    let asset_detail = if pane.custom_icon.is_some() {
        "A reusable user-uploaded image is selected"
    } else if definition.asset.is_some() {
        "Bundled official artwork is shown unchanged for referential identification only; no affiliation or endorsement is implied"
    } else if pane.identity.profile == TerminalProfile::GitHubCopilot {
        "The official CLI package exposes no standalone icon asset, so Harness Harlot uses the neutral terminal glyph"
    } else {
        "Harness Harlot uses the neutral terminal glyph"
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

pub(crate) const IDENTITY_MARK_SIZE: f32 = 22.0;

pub(crate) const OFFICIAL_IDENTITY_ICON_SIZE: f32 = 20.0;

pub(crate) const FALLBACK_IDENTITY_ICON_SIZE: f32 = 14.0;

const FALLBACK_IDENTITY_FRAME_SIZE: f32 = 18.0;

pub(crate) fn terminal_profile_icon_is_framed(profile: TerminalProfile) -> bool {
    agent_icon_definition(profile).asset.is_none()
}

pub(crate) fn terminal_profile_icon_size(profile: TerminalProfile) -> f32 {
    if terminal_profile_icon_is_framed(profile) {
        FALLBACK_IDENTITY_ICON_SIZE
    } else {
        OFFICIAL_IDENTITY_ICON_SIZE
    }
}

pub(crate) fn render_terminal_profile_mark(
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

pub(crate) fn render_sidebar_toggle_icon(sidebar_visible: bool) -> AnyElement {
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

pub(crate) fn render_bell_icon(color: u32) -> AnyElement {
    div()
        .relative()
        .w(px(14.0))
        .h(px(14.0))
        .child(
            div()
                .absolute()
                .left(px(3.0))
                .top(px(1.0))
                .w(px(8.0))
                .h(px(8.0))
                .rounded_tl(px(4.0))
                .rounded_tr(px(4.0))
                .border_1()
                .border_color(rgb(color)),
        )
        .child(
            div()
                .absolute()
                .left(px(1.0))
                .top(px(9.0))
                .w(px(12.0))
                .h(px(1.0))
                .bg(rgb(color)),
        )
        .child(
            div()
                .absolute()
                .left(px(6.0))
                .top(px(11.0))
                .w(px(2.0))
                .h(px(2.0))
                .rounded_full()
                .bg(rgb(color)),
        )
        .into_any_element()
}

pub(crate) fn render_terminal_profile_icon(
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

pub(crate) fn resolved_terminal_accent(
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

pub(crate) fn resolved_workspace_color(
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

pub(crate) fn workspace_is_selectable(workspace: &Workspace) -> bool {
    workspace.tabs.is_empty()
        || !matches!(
            workspace.connection,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            }
        )
}

#[cfg(test)]
mod tests {
    use super::{
        AppearanceColor, FALLBACK_IDENTITY_ICON_SIZE, OFFICIAL_IDENTITY_ICON_SIZE, Pane,
        SessionSnapshot, TerminalProfile, Uuid, WorkspaceConnection, WorkspaceConnectionStatus,
        agent_icon_definition, resolved_terminal_accent, resolved_workspace_color,
        tab_identity_presentation, terminal_profile_icon_is_framed, terminal_profile_icon_size,
        workspace_is_selectable,
    };
    use crate::helpers::{terminal_tab_secondary_label, visible_panes};
    use hh_protocol::PaneLayout;

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
                kind: hh_protocol::PaneKind::Terminal,
                title: label.to_owned(),
                shell: "zsh".to_owned(),
                color: None,
                identity: hh_protocol::TerminalIdentity {
                    profile,
                    source: if profile == TerminalProfile::Terminal {
                        hh_protocol::TerminalIdentitySource::Fallback
                    } else {
                        hh_protocol::TerminalIdentitySource::Command
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
            kind: hh_protocol::PaneKind::Terminal,
            title: "Release terminal".to_owned(),
            shell: "ssh release@long-production-host.example.com".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
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
}
