use hh_protocol::{PaneStatus, TerminalProfile};

/// Explicit cross-harness contract: emit OSC 777
/// `\x1b]777;notify;hh-status;<state>\x1b\\`.
/// The terminal scanner flattens that event to `hh-status: <state>`.
pub(crate) fn contract_status(message: &str) -> Option<PaneStatus> {
    match message {
        "hh-status: working" => Some(PaneStatus::Working),
        "hh-status: needs-approval" => Some(PaneStatus::NeedsApproval),
        "hh-status: needs-input" => Some(PaneStatus::NeedsInput),
        "hh-status: done" => Some(PaneStatus::Done),
        "hh-status: idle" => Some(PaneStatus::Idle),
        _ => None,
    }
}

/// Best-effort per-profile classification of OSC-9/777 notification text.
pub(crate) fn heuristic_status(profile: TerminalProfile, message: &str) -> Option<PaneStatus> {
    match profile {
        TerminalProfile::Omp if message.contains("Waiting for input") => {
            Some(PaneStatus::NeedsInput)
        }
        TerminalProfile::Omp if message.contains("Complete") => Some(PaneStatus::Done),
        TerminalProfile::Codex
            if message.starts_with("Approval requested")
                || message.starts_with("Codex wants to edit") =>
        {
            Some(PaneStatus::NeedsApproval)
        }
        TerminalProfile::Codex if message.starts_with("Agent turn complete") => {
            Some(PaneStatus::Done)
        }
        _ => None,
    }
}

/// Classifies the state marker in omp's `π > …`, `π <spinner> …`, and
/// `π ! …` terminal titles.
pub(crate) fn omp_title_status(title: &str) -> Option<PaneStatus> {
    match title.strip_prefix("π ")?.chars().next()? {
        '>' => Some(PaneStatus::Idle),
        '!' => Some(PaneStatus::NeedsApproval),
        '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇' | '⠏' => {
            Some(PaneStatus::Working)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_status_accepts_only_exact_contract_messages() {
        let cases = [
            ("hh-status: working", Some(PaneStatus::Working)),
            ("hh-status: needs-approval", Some(PaneStatus::NeedsApproval)),
            ("hh-status: needs-input", Some(PaneStatus::NeedsInput)),
            ("hh-status: done", Some(PaneStatus::Done)),
            ("hh-status: idle", Some(PaneStatus::Idle)),
            ("hh-status: unknown", None),
            ("prefix hh-status: working", None),
        ];
        for (message, expected) in cases {
            assert_eq!(contract_status(message), expected, "message: {message}");
        }
    }

    #[test]
    fn heuristics_are_profile_specific_and_bounded() {
        assert_eq!(
            heuristic_status(TerminalProfile::Omp, "Waiting for input"),
            Some(PaneStatus::NeedsInput)
        );
        assert_eq!(
            heuristic_status(TerminalProfile::Omp, "Task Complete"),
            Some(PaneStatus::Done)
        );
        assert_eq!(
            heuristic_status(TerminalProfile::Codex, "Approval requested: edit"),
            Some(PaneStatus::NeedsApproval)
        );
        assert_eq!(
            heuristic_status(TerminalProfile::Codex, "Codex wants to edit src/lib.rs"),
            Some(PaneStatus::NeedsApproval)
        );
        assert_eq!(
            heuristic_status(TerminalProfile::Codex, "Agent turn complete: fixed"),
            Some(PaneStatus::Done)
        );
        assert_eq!(
            heuristic_status(TerminalProfile::Codex, "anything else"),
            None
        );
        assert_eq!(
            heuristic_status(TerminalProfile::Terminal, "Waiting for input"),
            None
        );
    }

    #[test]
    fn omp_titles_map_idle_attention_and_all_spinner_frames() {
        assert_eq!(omp_title_status("π > ready"), Some(PaneStatus::Idle));
        assert_eq!(
            omp_title_status("π ! approval"),
            Some(PaneStatus::NeedsApproval)
        );
        for spinner in "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars() {
            assert_eq!(
                omp_title_status(&format!("π {spinner} working")),
                Some(PaneStatus::Working)
            );
        }
        assert_eq!(omp_title_status("π ? unknown"), None);
        assert_eq!(omp_title_status("omp > ready"), None);
    }
}
