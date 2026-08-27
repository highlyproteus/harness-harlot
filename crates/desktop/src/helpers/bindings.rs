use crate::commands::{AppCommand, ROOT_KEY_CONTEXT, ResolvedBinding};
use crate::view_models::SplitControlId;
use crate::{
    DEVELOPMENT_PRODUCT_NAME, EqualizePanes, FocusDown, FocusLeft, FocusRight, FocusUp,
    NewBrowserTab, NewTab, NewWorkspace, ReattachPane, STABLE_PRODUCT_NAME, ShowCommandPalette,
    ShowNotifications, ShowSettings, SplitDown, SplitRight, TerminalZoomIn, TerminalZoomOut,
    TogglePaneZoom, ToggleSidebar, ToggleVoiceMic,
};
use gpui::KeyBinding;
use uuid::Uuid;

pub(crate) fn abbreviate_home(path: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.to_owned();
    };
    if path == home {
        return "~".to_owned();
    }
    path.strip_prefix(&home)
        .filter(|suffix| suffix.starts_with('/'))
        .map_or_else(|| path.to_owned(), |suffix| format!("~{suffix}"))
}

pub(crate) fn expand_home(path: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.to_owned();
    };
    if path == "~" {
        return home;
    }
    path.strip_prefix("~/")
        .map_or_else(|| path.to_owned(), |suffix| format!("{home}/{suffix}"))
}

pub(crate) fn element_key(id: Uuid) -> u64 {
    let (high, low) = id.as_u64_pair();
    high ^ low
}

pub(crate) fn split_element_key(id: SplitControlId) -> u64 {
    element_key(id.first).rotate_left(17) ^ element_key(id.second)
}

pub(crate) fn gpui_binding(binding: &ResolvedBinding) -> KeyBinding {
    match binding.command {
        AppCommand::NewWorkspace => {
            KeyBinding::new(&binding.sequence, NewWorkspace, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ToggleSidebar => {
            KeyBinding::new(&binding.sequence, ToggleSidebar, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::NewTab => KeyBinding::new(&binding.sequence, NewTab, Some(ROOT_KEY_CONTEXT)),
        AppCommand::NewBrowserTab => {
            KeyBinding::new(&binding.sequence, NewBrowserTab, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::TerminalZoomIn => {
            KeyBinding::new(&binding.sequence, TerminalZoomIn, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::TerminalZoomOut => {
            KeyBinding::new(&binding.sequence, TerminalZoomOut, Some(ROOT_KEY_CONTEXT))
        }
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
        AppCommand::ShowNotifications => {
            KeyBinding::new(&binding.sequence, ShowNotifications, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ToggleVoiceMic => {
            KeyBinding::new(&binding.sequence, ToggleVoiceMic, Some(ROOT_KEY_CONTEXT))
        }
        AppCommand::ShowSettings => {
            KeyBinding::new(&binding.sequence, ShowSettings, Some(ROOT_KEY_CONTEXT))
        }
    }
}

pub(crate) fn product_name(development_build: bool) -> &'static str {
    if development_build {
        DEVELOPMENT_PRODUCT_NAME
    } else {
        STABLE_PRODUCT_NAME
    }
}

pub(crate) fn append_rename_text(value: &mut String, replace_on_type: &mut bool, text: &str) {
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
#[cfg(test)]
mod tests {
    use super::{append_rename_text, expand_home};

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
    fn home_expansion_only_rewrites_home_shorthand() {
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expand_home("~"), home);
            assert_eq!(expand_home("~/Projects"), format!("{home}/Projects"));
        }
        assert_eq!(expand_home("~someone/Projects"), "~someone/Projects");
        assert_eq!(expand_home("/tmp/project"), "/tmp/project");
    }
}
