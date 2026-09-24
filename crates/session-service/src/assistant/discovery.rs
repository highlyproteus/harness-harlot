use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hh_protocol::{CodingAgent, TERMINAL_PROFILE_REGISTRY, TerminalProfile};

use crate::process::{configured_shell, is_trusted_executable_file, run_bounded_command};

const PI_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
const MINIMUM_PI_VERSION: (u16, u16, u16) = (0, 85, 0);
pub(crate) const CODING_AGENT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub(crate) struct PiInstall {
    pub(crate) program: PathBuf,
    pub(crate) login_path: OsString,
    pub(crate) version: (u16, u16, u16),
}

pub(crate) fn discover_pi() -> Result<PiInstall> {
    let (program, login_path) = if let Some(program) = std::env::var_os("HH_PI_BINARY") {
        (
            PathBuf::from(program),
            std::env::var_os("PATH").unwrap_or_default(),
        )
    } else {
        let mut command = Command::new(configured_shell());
        command
            .args(["-lc", "command -v pi; printf '%s\\n' \"$PATH\""])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_bounded_command(command, PI_DISCOVERY_TIMEOUT, "pi discovery")?;
        let mut lines = output.stdout.lines();
        let program = lines.next().unwrap_or_default().trim();
        if program.is_empty() {
            bail!(
                "pi is not installed. Install it with: curl -fsSL https://pi.dev/install.sh | sh"
            );
        }
        let program = PathBuf::from(program);
        if !program.is_absolute() {
            bail!(
                "pi discovery returned a non-absolute path: {}",
                program.display()
            );
        }
        let login_path = OsString::from(lines.next().unwrap_or_default());
        (program, login_path)
    };

    let program = program.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "pi is not installed. Install it with: curl -fsSL https://pi.dev/install.sh | sh ({error})"
        )
    })?;
    if !is_trusted_executable_file(&program) {
        bail!(
            "pi executable is not a trusted executable file: {}",
            program.display()
        );
    }

    let mut command = Command::new(&program);
    command
        .arg("--version")
        .env("PATH", &login_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded_command(command, PI_DISCOVERY_TIMEOUT, "pi version probe")?;
    if !output.success {
        let message = output.stderr.trim();
        bail!(
            "pi version probe failed{}",
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        );
    }
    let raw_version = output.stdout.lines().next().unwrap_or_default().trim();
    let version = parse_pi_version(raw_version)?;
    if version < MINIMUM_PI_VERSION {
        bail!(
            "pi {} is too old; Harness Harlot needs pi 0.85 or newer",
            display_version(version)
        );
    }

    Ok(PiInstall {
        program,
        login_path,
        version,
    })
}

fn parse_pi_version(value: &str) -> Result<(u16, u16, u16)> {
    let value = value.strip_prefix("pi ").unwrap_or(value);
    let value = value.strip_prefix('v').unwrap_or(value);
    let core = value.split(['-', '+']).next().unwrap_or(value);
    let mut components = core.split('.');
    let major = parse_component(components.next(), value)?;
    let minor = parse_component(components.next(), value)?;
    let patch = parse_component(components.next(), value)?;
    if components.next().is_some() {
        bail!("pi reported an invalid version: {value}");
    }
    Ok((major, minor, patch))
}

fn parse_component(component: Option<&str>, original: &str) -> Result<u16> {
    component
        .filter(|value| !value.is_empty())
        .context("missing version component")?
        .parse::<u16>()
        .with_context(|| format!("pi reported an invalid version: {original}"))
}

fn display_version(version: (u16, u16, u16)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

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
    fn parses_supported_pi_version_forms() {
        assert_eq!(parse_pi_version("0.85.0").unwrap(), (0, 85, 0));
        assert_eq!(parse_pi_version("v1.2.3").unwrap(), (1, 2, 3));
        assert_eq!(parse_pi_version("pi 0.90.1").unwrap(), (0, 90, 1));
    }

    #[test]
    fn rejects_incomplete_pi_versions() {
        assert!(parse_pi_version("0.85").is_err());
        assert!(parse_pi_version("pi latest").is_err());
    }

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
