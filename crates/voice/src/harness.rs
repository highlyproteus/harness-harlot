use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Agent {
    Omp,
    Hermes,
    Codex,
    Claude,
}

impl Agent {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "omp" => Ok(Self::Omp),
            "hermes" => Ok(Self::Hermes),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => bail!("agent must be one of omp, hermes, codex, or claude"),
        }
    }
}

/// Returns the interactive launch command for a supported coding harness.
///
/// Harnesses can publish stable state by emitting OSC 777
/// `\x1b]777;notify;hh-status;<state>\x1b\\`, where state is `working`,
/// `needs-approval`, `needs-input`, `done`, or `idle`. A future Hermes patch
/// should emit turn completion at `hermes-agent/cli.py:14617` and approval
/// state around its approval panel at `hermes-agent/cli.py:13620`.
pub(crate) const fn launch_command(agent: Agent) -> &'static str {
    match agent {
        Agent::Omp => "omp",
        Agent::Hermes => "hermes",
        Agent::Codex => {
            "codex -c tui.notification_method=osc9 -c tui.notification_condition=always"
        }
        Agent::Claude => "claude",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_launch_interactively_without_task_arguments() {
        assert_eq!(launch_command(Agent::Omp), "omp");
        assert_eq!(launch_command(Agent::Hermes), "hermes");
        assert_eq!(
            launch_command(Agent::Codex),
            "codex -c tui.notification_method=osc9 -c tui.notification_condition=always"
        );
        assert_eq!(launch_command(Agent::Claude), "claude");
    }
}
