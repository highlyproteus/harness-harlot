use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use hh_protocol::{CodingAgent, TERMINAL_PROFILE_REGISTRY, TerminalProfile};

use crate::process::{configured_shell, is_trusted_executable_file, run_bounded_command};

const CODING_AGENT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

/// Every registry command that can host a coding agent, in registry order.
/// tmux is a multiplexer, not a coding agent, and is excluded.
fn coding_agent_candidates() -> Vec<(TerminalProfile, &'static str)> {
    TERMINAL_PROFILE_REGISTRY
        .iter()
        .filter(|definition| definition.profile != TerminalProfile::Tmux)
        .flat_map(|definition| {
            definition
                .commands
                .iter()
                .map(move |command| (definition.profile, *command))
        })
        .collect()
}

/// Login `PATH`, probed once per process. Coding-agent discovery and rescans
/// then resolve candidates in-process instead of paying for another login
/// shell, which on a heavy shell profile costs about a second each time.
///
/// `OnceLock`, not `LazyLock`: only a successful probe is cached, so a broken
/// or timed-out login shell stays retryable through Rescan.
fn login_path() -> Result<&'static OsString> {
    static LOGIN_PATH: OnceLock<OsString> = OnceLock::new();
    if let Some(path) = LOGIN_PATH.get() {
        return Ok(path);
    }
    let mut command = Command::new(configured_shell());
    command
        .args(["-lc", "printf '%s' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded_command(
        command,
        CODING_AGENT_DISCOVERY_TIMEOUT,
        "login PATH discovery",
    )?;
    Ok(LOGIN_PATH.get_or_init(|| OsString::from(output.stdout.trim())))
}

/// Resolves installed coding agent CLIs on the login `PATH`. An empty result
/// is valid: it means no supported agent is installed.
pub(crate) fn discover_coding_agents() -> Result<Vec<CodingAgent>> {
    let login_path = login_path()?;
    let directories = std::env::split_paths(login_path).collect::<Vec<_>>();
    let mut agents: Vec<CodingAgent> = Vec::new();
    for (profile, name) in coding_agent_candidates() {
        if agents.iter().any(|agent| agent.profile == profile) {
            continue;
        }
        let resolved = directories
            .iter()
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join(name))
            .filter_map(|candidate| candidate.canonicalize().ok())
            .find(|candidate| is_trusted_executable_file(candidate));
        if let Some(path) = resolved {
            agents.push(CodingAgent {
                profile,
                command: name.to_owned(),
                path: path.to_string_lossy().into_owned(),
            });
        }
    }
    Ok(agents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_agent_candidates_skip_tmux_and_keep_registry_order() {
        let candidates = coding_agent_candidates();
        assert_eq!(
            candidates.first(),
            Some(&(TerminalProfile::Hermes, "hermes"))
        );
        assert!(
            candidates
                .iter()
                .all(|(profile, _)| *profile != TerminalProfile::Tmux)
        );
        let position = |needle: TerminalProfile| {
            candidates
                .iter()
                .position(|(profile, _)| *profile == needle)
                .expect("profile is a discovery candidate")
        };
        assert!(position(TerminalProfile::Omp) < position(TerminalProfile::Codex));
    }
}
