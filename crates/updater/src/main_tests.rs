use super::*;

#[test]
fn production_network_updates_require_packaged_build_metadata() {
    assert!(!production_update_eligible(0));
    assert!(production_update_eligible(1));
}

#[test]
fn production_updates_require_the_native_platform_and_architecture() {
    assert!(ensure_production_target_matches(false, "arm64", "arm64", "architecture").is_ok());
    assert!(ensure_production_target_matches(false, "x86_64", "arm64", "architecture").is_err());
    assert!(ensure_production_target_matches(false, "linux", "macos", "platform").is_err());
    assert!(ensure_production_target_matches(true, "x86_64", "arm64", "architecture").is_ok());
}

#[test]
fn macos_install_prefix_follows_the_bundle_that_contains_the_updater() {
    let updater = Path::new("/Applications/Harness Harlot.app/Contents/MacOS/hh-update-tool");
    assert_eq!(
        macos_install_prefix_for_executable(updater),
        Some(PathBuf::from("/Applications"))
    );
    assert_eq!(
        macos_install_prefix_for_executable(Path::new("/tmp/hh-update-tool")),
        None
    );
}

#[test]
#[cfg(target_os = "macos")]
fn macos_relaunch_forces_a_new_instance_of_the_installed_bundle() {
    let app = Path::new("/Applications/Harness Harlot.app");
    assert_eq!(macos_relaunch_program(false), "/usr/bin/open");
    assert_eq!(macos_relaunch_program(true), "open");
    assert_eq!(
        macos_relaunch_arguments(app),
        vec!["-n".as_ref(), app.as_os_str()]
    );
}

#[test]
fn desktop_process_detection_does_not_block_the_calling_cli() {
    assert!(command_line_is_desktop(&[
        "/Applications/Harness Harlot.app/Contents/MacOS/hh"
    ]));
    assert!(!command_line_is_desktop(&[
        "/Applications/Harness Harlot.app/Contents/MacOS/hh",
        "update",
    ]));
    assert!(!command_line_is_desktop(&["hh", "version"]));
}

#[test]
#[cfg(target_os = "macos")]
fn staged_directory_guard_cleans_an_abandoned_bundle() {
    let temporary = TemporaryDirectory::new().unwrap();
    let staging = temporary.path.join(".Harness Harlot.app.new.test");
    fs::create_dir(&staging).unwrap();
    {
        let _cleanup = StagedDirectoryGuard::new(&staging);
    }
    assert!(!staging.exists());
}

#[test]
fn confined_paths_resolve_existing_symlink_ancestors() {
    let temporary = TemporaryDirectory::new().unwrap();
    let home = temporary.path.join("home");
    let outside = temporary.path.join("outside");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&outside).unwrap();

    assert!(path_is_confined(&home.join(".local/lib/new"), &home).unwrap());
    symlink(&outside, home.join("escape")).unwrap();
    assert!(!path_is_confined(&home.join("escape/new"), &home).unwrap());
    assert!(!path_is_confined(&home.join("../outside"), &home).unwrap());
}

#[test]
fn linux_install_rejects_unlisted_empty_directories() {
    let temporary = TemporaryDirectory::new().unwrap();
    let app = temporary.path.join("app");
    fs::create_dir(&app).unwrap();
    fs::create_dir(app.join("unexpected")).unwrap();

    let error = validate_linux_install(&app).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("application contains unexpected directory unexpected")
    );
}

#[test]
fn linux_browser_runtime_requires_helper_data_and_locale() {
    let mut files = HashSet::from(["bin/libcef.so".to_owned()]);
    assert!(validate_linux_cef_files(&files).is_err());

    files.extend(
        [
            "bin/hh-cef-helper",
            "bin/icudtl.dat",
            "bin/v8_context_snapshot.bin",
            "bin/locales/en-US.pak",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    validate_linux_cef_files(&files).unwrap();
}

#[test]
fn desktop_update_handoff_requires_complete_process_identity() {
    let complete = [
        "install",
        "--current-version",
        "0.1.17",
        "--wait-pid",
        "42",
        "--wait-start-time",
        "84",
    ]
    .map(str::to_owned);
    assert_eq!(desktop_update_handoff_identity(&complete), Some((42, 84)));
    assert_eq!(
        desktop_update_relaunch_target(
            &complete,
            Path::new("/Applications/Harness Harlot.app/Contents/MacOS/hh-update-tool")
        ),
        Some((42, 84, PathBuf::from("/Applications/Harness Harlot.app")))
    );

    let cli = ["install", "--current-version", "0.1.17"].map(str::to_owned);
    assert_eq!(desktop_update_handoff_identity(&cli), None);

    let incomplete = ["install", "--wait-pid", "42"].map(str::to_owned);
    assert_eq!(desktop_update_handoff_identity(&incomplete), None);

    let check = ["check", "--wait-pid", "42", "--wait-start-time", "84"].map(str::to_owned);
    assert_eq!(desktop_update_handoff_identity(&check), None);
}

#[test]
fn process_identity_includes_start_time() {
    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
    let start_time = system.process(pid).unwrap().start_time();

    assert!(process_matches_start_time(&system, pid, start_time));
    assert!(!process_matches_start_time(
        &system,
        pid,
        start_time.wrapping_add(1)
    ));
}
