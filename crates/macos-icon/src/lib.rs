//! Minimal macOS `AppKit` and `Foundation` integration for the desktop process.
#![cfg(target_os = "macos")]

use objc2::AnyThread as _;
use objc2_app_kit::{NSApplication, NSDockTile, NSImage};
use objc2_foundation::{
    NSBundle, NSNumber, NSString, NSURL, NSURLIsExcludedFromBackupKey, ns_string,
};

// SAFETY: `NSApp` is AppKit's process-global application pointer.
#[allow(unsafe_code)]
unsafe extern "C" {
    static NSApp: Option<&'static NSApplication>;
}

/// Loads the selected packaged ICNS and assigns it to the live `AppKit` application.
pub fn install_dock_icon(development_build: bool) {
    let icon_name = if development_build {
        "Harness-Harlot-Dev"
    } else {
        "Harness-Harlot"
    };
    let Some(icon_path) = NSBundle::mainBundle().pathForResource_ofType(
        Some(&NSString::from_str(icon_name)),
        Some(ns_string!("icns")),
    ) else {
        eprintln!("Harness Harlot could not locate its bundled macOS icon");
        return;
    };
    let Some(icon) = NSImage::initWithContentsOfFile(NSImage::alloc(), &icon_path) else {
        eprintln!("Harness Harlot could not load its bundled macOS icon");
        return;
    };

    // SAFETY: AppKit owns the shared application instance and retains the image.
    #[allow(unsafe_code)]
    unsafe {
        let Some(app) = NSApp else {
            eprintln!("Harness Harlot could not access the macOS application instance");
            return;
        };
        app.setApplicationIconImage(Some(&icon));
    }
}

/// Updates the running application's Dock tile badge.
pub fn set_dock_badge(label: Option<&str>) {
    let label = label.map(NSString::from_str);
    // SAFETY: AppKit owns the shared application and Dock tile. AppKit copies
    // the NSString badge value before this function returns.
    #[allow(unsafe_code)]
    unsafe {
        let Some(app) = NSApp else {
            eprintln!("Harness Harlot could not access the macOS application instance");
            return;
        };
        let dock_tile: objc2::rc::Retained<NSDockTile> = app.dockTile();
        dock_tile.setBadgeLabel(label.as_deref());
    }
}

/// Excludes an existing local-history directory from Time Machine snapshots.
///
/// # Errors
///
/// Returns the Foundation error when macOS refuses to update the resource key.
pub fn exclude_directory_from_backup(path: &std::path::Path) -> Result<(), String> {
    let path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath_isDirectory(&path, true);
    let excluded = NSNumber::new_bool(true);
    // SAFETY: NSURLIsExcludedFromBackupKey requires an NSNumber boolean value.
    #[allow(unsafe_code)]
    unsafe {
        url.setResourceValue_forKey_error(Some(&excluded), NSURLIsExcludedFromBackupKey)
            .map_err(|error| error.localizedDescription().to_string())
    }
}
