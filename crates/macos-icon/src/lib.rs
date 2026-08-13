//! The Dock icon setter is the only macOS-specific unsafe boundary in the app.

use objc2::AnyThread as _;
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::{NSBundle, NSString, ns_string};

// SAFETY: `NSApp` is AppKit's process-global application pointer.
#[allow(unsafe_code)]
unsafe extern "C" {
    static NSApp: Option<&'static NSApplication>;
}

/// Loads the selected packaged ICNS and assigns it to the live `AppKit` application.
pub fn install_dock_icon(development_build: bool) {
    let icon_name = if development_build {
        "Not-a-Harness-Dev"
    } else {
        "Not-a-Harness"
    };
    let Some(icon_path) = NSBundle::mainBundle().pathForResource_ofType(
        Some(&NSString::from_str(icon_name)),
        Some(ns_string!("icns")),
    ) else {
        eprintln!("Not a Harness could not locate its bundled macOS icon");
        return;
    };
    let Some(icon) = NSImage::initWithContentsOfFile(NSImage::alloc(), &icon_path) else {
        eprintln!("Not a Harness could not load its bundled macOS icon");
        return;
    };

    // SAFETY: AppKit owns the shared application instance and retains the image.
    #[allow(unsafe_code)]
    unsafe {
        let Some(app) = NSApp else {
            eprintln!("Not a Harness could not access the macOS application instance");
            return;
        };
        app.setApplicationIconImage(Some(&icon));
    }
}
