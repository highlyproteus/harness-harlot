//! Desktop handoff identity and managed service shutdown.
use std::fs;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use sysinfo::{Pid, ProcessesToUpdate, System};

use super::path_exists;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ServiceRestart {
    Keep,
    WhenQuiescent,
    Forced,
}
pub(super) fn wait_for_process_exit(process_id: u32, start_time: u64) -> Result<()> {
    ensure!(
        process_id != std::process::id(),
        "installer cannot wait for itself"
    );
    let pid = Pid::from_u32(process_id);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut system = System::new();
    loop {
        // Targeted refreshes retain dead entries in sysinfo's process map.
        // A zero refresh count, not the cached entry, proves this PID exited.
        let refreshed = system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
        if refreshed == 0 || !process_matches_start_time(&system, pid, start_time) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "desktop process {process_id} did not exit before update"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn command_line_is_desktop(command: &[&str]) -> bool {
    command.len() == 1
        || command.get(1..).is_some_and(|arguments| {
            arguments
                .iter()
                .all(|argument| argument.starts_with("-psn_"))
        })
}

pub(super) fn ensure_no_running_desktop_process() -> Result<()> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All);
    let running = system.processes().values().any(|process| {
        if process.name().to_string_lossy() != "hh" {
            return false;
        }
        let command = process
            .cmd()
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        let command = command.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        command_line_is_desktop(&command)
    });
    ensure!(
        !running,
        "quit Harness Harlot before updating; the app will restart after installation"
    );
    Ok(())
}

pub(super) fn process_matches_start_time(system: &System, pid: Pid, start_time: u64) -> bool {
    system
        .process(pid)
        .is_some_and(|process| process.start_time() == start_time)
}
pub(super) fn stop_managed_service(service: &Path, restart: ServiceRestart) -> Result<()> {
    if restart == ServiceRestart::Keep {
        return Ok(());
    }
    let socket = hh_protocol::socket_path()?;
    if StdUnixStream::connect(&socket).is_err() {
        return Ok(());
    }
    let status = Command::new(service)
        .arg("--shutdown")
        .status()
        .with_context(|| format!("request shutdown through {}", service.display()))?;
    if status.success() {
        return wait_for_service_stop(&socket, Duration::from_secs(5));
    }
    ensure!(
        restart == ServiceRestart::Forced,
        "session service still owns live terminals; re-run with --restart-service to restart it (tmux-managed local shells resume; fallback shells restart) or close every terminal first"
    );

    let managed_service = fs::canonicalize(service)
        .with_context(|| format!("resolve managed session service {}", service.display()))?;
    let system = System::new_all();
    let service_processes = system
        .processes()
        .values()
        .filter(|process| {
            process
                .exe()
                .and_then(|executable| fs::canonicalize(executable).ok())
                .is_some_and(|executable| executable == managed_service)
        })
        .map(sysinfo::Process::pid)
        .collect::<Vec<_>>();
    ensure!(
        !service_processes.is_empty(),
        "could not find the running session service to restart"
    );
    for pid in service_processes {
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid.as_u32())?)
            .context("session service has an invalid process ID")?;
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).with_context(|| {
            format!(
                "send SIGTERM to session service process {}",
                pid.as_raw_pid()
            )
        })?;
    }
    wait_for_service_socket_removal(&socket)
}

fn wait_for_service_socket_removal(socket: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while path_exists(socket)? {
        ensure!(
            Instant::now() < deadline,
            "session service did not stop after SIGTERM"
        );
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn wait_for_service_stop(socket: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while StdUnixStream::connect(socket).is_ok() {
        ensure!(
            Instant::now() < deadline,
            "session service did not stop before update"
        );
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
