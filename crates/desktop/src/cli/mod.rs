mod agent;
mod args;
pub(crate) mod mcp;
pub(crate) mod skill;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use args::{AgentAction, AgentCommand, SkillCommand};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdateOptions {
    pub(crate) check_only: bool,
    pub(crate) restart_service: bool,
    pub(crate) channel: hh_updater::UpdateChannel,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CliAction {
    LaunchDesktop,
    Update(UpdateOptions),
    Version,
    Doctor,
    InstallCli,
    Agent(AgentCommand),
}

pub(crate) fn parse_cli_action<I, S>(arguments: I) -> Result<CliAction>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => Ok(CliAction::LaunchDesktop),
        [argument] if argument == "version" || argument == "--version" => Ok(CliAction::Version),
        [argument] if argument == "doctor" => Ok(CliAction::Doctor),
        [argument] if argument == "install-cli" => Ok(CliAction::InstallCli),
        [command, arguments @ ..] if command == "update" => {
            parse_update_options(arguments).map(CliAction::Update)
        }
        [command, arguments @ ..]
            if matches!(command.as_str(), "browser" | "gallery" | "mcp" | "skill") =>
        {
            let mut agent_arguments = Vec::with_capacity(arguments.len() + 1);
            agent_arguments.push(command.clone());
            agent_arguments.extend_from_slice(arguments);
            args::parse_agent_command(&agent_arguments).map(CliAction::Agent)
        }
        _ => bail!("unknown Harness Harlot command or arguments"),
    }
}

fn parse_update_options(arguments: &[String]) -> Result<UpdateOptions> {
    let mut check_only = false;
    let mut restart_service = false;
    let mut channel = hh_updater::UpdateChannel::Stable;
    let mut channel_set = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--check" => {
                ensure!(!check_only, "--check may be specified only once");
                check_only = true;
            }
            "--restart-service" => {
                ensure!(
                    !restart_service,
                    "--restart-service may be specified only once"
                );
                restart_service = true;
            }
            "--channel" => {
                ensure!(!channel_set, "--channel may be specified only once");
                index += 1;
                channel = parse_channel(
                    arguments
                        .get(index)
                        .context("missing value for --channel")?,
                )?;
                channel_set = true;
            }
            _ => bail!("unknown Harness Harlot command or arguments"),
        }
        index += 1;
    }
    ensure!(
        !(check_only && restart_service),
        "--restart-service cannot be used with --check"
    );
    Ok(UpdateOptions {
        check_only,
        restart_service,
        channel,
    })
}

fn parse_channel(channel: &str) -> Result<hh_updater::UpdateChannel> {
    match channel {
        "stable" => Ok(hh_updater::UpdateChannel::Stable),
        "edge" => Ok(hh_updater::UpdateChannel::Edge),
        _ => bail!("update channel must be stable or edge"),
    }
}

fn updater_arguments(options: &UpdateOptions, version: &str, build: u64) -> Vec<String> {
    let mut arguments = vec![
        if options.check_only {
            "check"
        } else {
            "install"
        }
        .to_owned(),
        "--current-version".to_owned(),
        version.to_owned(),
        "--current-build".to_owned(),
        build.to_string(),
        "--channel".to_owned(),
        options.channel.as_str().to_owned(),
    ];
    if options.restart_service {
        arguments.push("--restart-service".to_owned());
    }
    arguments
}

fn bundled_executable(name: &str) -> Result<PathBuf> {
    let current = std::env::current_exe().context("resolve the Harness Harlot executable")?;
    bundled_executable_from(&current, name)
}

fn bundled_executable_from(current: &Path, name: &str) -> Result<PathBuf> {
    let current = fs::canonicalize(current)
        .with_context(|| format!("resolve Harness Harlot executable {}", current.display()))?;
    let candidate = current
        .parent()
        .context("the Harness Harlot executable has no parent directory")?
        .join(name);
    ensure!(candidate.is_file(), "bundled {name} is missing");
    Ok(candidate)
}

fn install_cli_link(current: &Path, home: &Path) -> Result<PathBuf> {
    let directory = home.join(".local/bin");
    fs::create_dir_all(&directory)
        .with_context(|| format!("create command directory {}", directory.display()))?;
    let link = directory.join("hh");
    if let Ok(metadata) = fs::symlink_metadata(&link) {
        ensure!(
            metadata.file_type().is_symlink(),
            "refusing to replace non-symlink command {}",
            link.display()
        );
        let target = fs::read_link(&link)
            .with_context(|| format!("read command link {}", link.display()))?;
        if target == current {
            return Ok(link);
        }
        bail!(
            "refusing to replace command link {} owned by another installation",
            link.display()
        );
    }
    symlink(current, &link).with_context(|| format!("create command link {}", link.display()))?;
    Ok(link)
}

pub(crate) fn ensure_packaged_cli_link() -> Result<Option<PathBuf>> {
    if hh_updater::current_build() == 0 {
        return Ok(None);
    }
    let current = std::env::current_exe().context("resolve Harness Harlot executable")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    install_cli_link(&current, &home).map(Some)
}

pub(crate) fn run_cli_or_request_desktop() -> Result<bool> {
    let action = parse_cli_action(std::env::args().skip(1))?;
    match action {
        CliAction::LaunchDesktop => return Ok(true),
        CliAction::Version => {
            println!(
                "Harness Harlot {} (build {})",
                env!("CARGO_PKG_VERSION"),
                hh_updater::current_build()
            );
        }
        CliAction::Update(options) => {
            ensure!(
                hh_updater::current_build() > 0,
                "updates are available only from a packaged Harness Harlot installation"
            );
            let tool = bundled_executable("hh-update-tool")?;
            let status = Command::new(&tool)
                .args(updater_arguments(
                    &options,
                    env!("CARGO_PKG_VERSION"),
                    hh_updater::current_build(),
                ))
                .status()
                .with_context(|| format!("run bundled updater {}", tool.display()))?;
            ensure!(status.success(), "update command failed with {status}");
        }
        CliAction::Doctor => {
            let current = std::env::current_exe().context("resolve Harness Harlot executable")?;
            let service = bundled_executable("hh-service")?;
            let updater = bundled_executable("hh-update-tool")?;
            println!("executable: {}", current.display());
            println!("version: {}", env!("CARGO_PKG_VERSION"));
            println!("build: {}", hh_updater::current_build());
            println!("session service: {}", service.display());
            println!("updater: {}", updater.display());
            println!("Harness Harlot installation is healthy");
        }
        CliAction::InstallCli => {
            ensure!(
                hh_updater::current_build() > 0,
                "the CLI command can be installed only from a packaged Harness Harlot app"
            );
            let current = std::env::current_exe().context("resolve Harness Harlot executable")?;
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")?;
            let link = install_cli_link(&current, &home)?;
            println!("installed {}", link.display());
        }
        CliAction::Agent(command) => match &command.action {
            AgentAction::Mcp => mcp::run(&command.context)?,
            AgentAction::Skill(SkillCommand::Install) => {
                for path in skill::install_default()? {
                    println!("installed {}", path.display());
                }
            }
            AgentAction::Skill(SkillCommand::Path) => {
                println!("{}", skill::source_path().display());
            }
            AgentAction::Browser(_) | AgentAction::Gallery(_) => {
                let result = agent::execute(&command)?;
                agent::print_result(&result, command.context.json)?;
            }
        },
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{
        CliAction, UpdateOptions, bundled_executable_from, install_cli_link, parse_cli_action,
        updater_arguments,
    };

    #[test]
    fn no_arguments_launches_the_desktop() {
        assert_eq!(
            parse_cli_action(std::iter::empty::<&str>()).unwrap(),
            CliAction::LaunchDesktop
        );
    }

    #[test]
    fn update_commands_are_dispatched_without_launching_the_desktop() {
        assert_eq!(
            parse_cli_action(["update"]).unwrap(),
            CliAction::Update(UpdateOptions {
                check_only: false,
                channel: hh_updater::UpdateChannel::Stable,
                restart_service: false,
            })
        );
        assert_eq!(
            parse_cli_action(["update", "--check"]).unwrap(),
            CliAction::Update(UpdateOptions {
                check_only: true,
                channel: hh_updater::UpdateChannel::Stable,
                restart_service: false,
            })
        );
        assert_eq!(
            parse_cli_action(["update", "--channel", "edge"]).unwrap(),
            CliAction::Update(UpdateOptions {
                check_only: false,
                channel: hh_updater::UpdateChannel::Edge,
                restart_service: false,
            })
        );
        assert_eq!(
            parse_cli_action(["update", "--restart-service", "--channel", "edge"]).unwrap(),
            CliAction::Update(UpdateOptions {
                check_only: false,
                channel: hh_updater::UpdateChannel::Edge,
                restart_service: true,
            })
        );
    }

    #[test]
    fn informational_commands_are_dispatched_without_launching_the_desktop() {
        assert_eq!(parse_cli_action(["version"]).unwrap(), CliAction::Version);
        assert_eq!(parse_cli_action(["--version"]).unwrap(), CliAction::Version);
        assert_eq!(parse_cli_action(["doctor"]).unwrap(), CliAction::Doctor);
        assert_eq!(
            parse_cli_action(["install-cli"]).unwrap(),
            CliAction::InstallCli
        );
    }

    #[test]
    fn unknown_or_extra_arguments_fail_closed() {
        assert!(parse_cli_action(["unknown"]).is_err());
        assert!(parse_cli_action(["version", "extra"]).is_err());
        assert!(parse_cli_action(["update", "--unknown"]).is_err());
        assert!(parse_cli_action(["update", "--check", "--restart-service"]).is_err());
    }

    #[test]
    fn update_handoff_includes_the_packaged_release_identity() {
        assert_eq!(
            updater_arguments(
                &UpdateOptions {
                    check_only: false,
                    channel: hh_updater::UpdateChannel::Stable,
                    restart_service: true,
                },
                "1.2.3",
                42,
            ),
            [
                "install",
                "--current-version",
                "1.2.3",
                "--current-build",
                "42",
                "--channel",
                "stable",
                "--restart-service"
            ]
        );
        assert_eq!(
            updater_arguments(
                &UpdateOptions {
                    check_only: true,
                    channel: hh_updater::UpdateChannel::Edge,
                    restart_service: false,
                },
                "1.2.3",
                42,
            ),
            [
                "check",
                "--current-version",
                "1.2.3",
                "--current-build",
                "42",
                "--channel",
                "edge"
            ]
        );
    }

    #[test]
    fn cli_link_is_idempotent_and_refuses_foreign_ownership() {
        let root = std::env::temp_dir().join(format!("hh-cli-link-test-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let app = root.join("Harness Harlot.app/Contents/MacOS");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        let executable = app.join("hh");
        std::fs::write(&executable, b"fixture").unwrap();

        let link = install_cli_link(&executable, &home).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), executable);
        assert_eq!(install_cli_link(&executable, &home).unwrap(), link);

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(root.join("foreign-hh"), &link).unwrap();
        assert!(install_cli_link(&executable, &home).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundled_executable_follows_the_installed_cli_symlink() {
        let root =
            std::env::temp_dir().join(format!("hh-cli-bundle-test-{}", uuid::Uuid::new_v4()));
        let bin = root.join("home/.local/bin");
        let bundle = root.join("Applications/Harness Harlot.app/Contents/MacOS");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&bundle).unwrap();
        let executable = bundle.join("hh");
        let updater = bundle.join("hh-update-tool");
        std::fs::write(&executable, b"fixture").unwrap();
        std::fs::write(&updater, b"fixture").unwrap();
        let link = bin.join("hh");
        std::os::unix::fs::symlink(&executable, &link).unwrap();

        assert_eq!(
            bundled_executable_from(&link, "hh-update-tool").unwrap(),
            std::fs::canonicalize(updater).unwrap()
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
