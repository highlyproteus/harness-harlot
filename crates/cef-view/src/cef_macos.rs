// The `cef` wrapper macros generate pointer transmutes internally; call sites
// cannot replace those macro-owned conversions.
#![allow(clippy::transmute_ptr_to_ptr)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, ensure};
use cef::{
    App, Browser, CefString, ImplBrowser as _, ImplBrowserHost as _, Settings, WindowInfo,
    api_hash, do_message_loop_work, execute_process, library_loader, sys,
};
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::ffi;
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{MainThreadMarker, ProtocolType as _, sel};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::BrowserRect;
use crate::cef_common::{self, INITIALIZED, TERMINATED, bounded_f64, cef_rect};

pub(crate) type ParentHandle = *mut c_void;

static PROTOCOL_INSTALL: Once = Once::new();
static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);
static LIBRARY: OnceLock<library_loader::LibraryLoader> = OnceLock::new();

pub(crate) fn ensure_main_thread() -> anyhow::Result<()> {
    ensure!(
        MainThreadMarker::new().is_some(),
        "CEF runtime must run on the macOS main thread"
    );
    Ok(())
}

pub(crate) fn create_window_info(parent: ParentHandle, rect: BrowserRect) -> WindowInfo {
    WindowInfo::default().set_as_child(parent.cast(), &cef_rect(rect))
}

pub(crate) fn apply_bounds(browser: &Browser, rect: BrowserRect, parent_height: f32) {
    let bounds = appkit_rect(rect, parent_height);
    with_native_view(browser, |view| view.setFrame(bounds));
}

pub(crate) fn set_visible(browser: &Browser, visible: bool) {
    with_native_view(browser, |view| view.setHidden(!visible));
}

pub(crate) fn focus_parent(parent: ParentHandle) {
    if parent.is_null() {
        return;
    }
    // SAFETY: The parent GPUI NSView and its NSWindow outlive every browser pane.
    #[allow(unsafe_code)]
    unsafe {
        let parent = parent.cast::<AnyObject>();
        let window: *mut AnyObject = msg_send![parent, window];
        if !window.is_null() {
            let _: Bool = msg_send![window, makeFirstResponder: parent];
        }
    }
}

/// Returns whether the bundled CEF framework is present beside this executable.
pub fn preflight() -> bool {
    framework_path().is_some_and(|path| path.is_dir())
}

/// Runs CEF's subprocess split before any application/UI initialization.
pub fn early_process_split() -> Option<i32> {
    if !preflight() {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let loader = library_loader::LibraryLoader::new(&executable, false);
    if !loader.load() {
        return None;
    }
    if api_hash(sys::CEF_API_VERSION_LAST, 0).is_null() {
        return None;
    }
    let args = cef::args::Args::new();
    let result = execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
    if result >= 0 {
        return Some(result);
    }
    let _ = LIBRARY.set(loader);
    None
}

/// Initializes the process-wide external-message-pump runtime on the main thread.
///
/// # Errors
///
/// Returns an error when called off the main thread, after shutdown, without a
/// loadable CEF framework and helper, or when cache or runtime initialization
/// fails.
pub fn init_runtime(cache_dir: &Path) -> anyhow::Result<()> {
    ensure_main_thread()?;
    ensure!(
        !TERMINATED.load(Ordering::Acquire),
        "CEF runtime cannot be initialized again after shutdown"
    );
    if INITIALIZED.load(Ordering::Acquire) {
        return Ok(());
    }
    ensure!(preflight(), "CEF framework is absent from the app bundle");
    ensure!(
        LIBRARY.get().is_some(),
        "CEF early process split did not run"
    );
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("create browser cache directory {}", cache_dir.display()))?;
    let cache_dir = cache_dir
        .canonicalize()
        .with_context(|| format!("resolve browser cache directory {}", cache_dir.display()))?;
    let subprocess_path =
        helper_executable_path().context("resolve bundled CEF helper executable")?;
    ensure!(
        subprocess_path.is_file(),
        "CEF helper executable is absent at {}",
        subprocess_path.display()
    );
    let settings = Settings {
        external_message_pump: 1,
        root_cache_path: CefString::from(cache_dir.to_string_lossy().as_ref()),
        ..Default::default()
    };
    cef_common::initialize_with_settings(&settings)?;
    schedule_message_pump();
    Ok(())
}

fn schedule_message_pump() {
    let when = DispatchTime::try_from(Duration::from_millis(10)).unwrap_or(DispatchTime::NOW);
    let _ = DispatchQueue::main().after(when, || {
        if INITIALIZED.load(Ordering::Acquire) {
            do_message_loop_work();
            schedule_message_pump();
        }
    });
}

/// Adds CEF's required application protocol methods to GPUI's `NSApplication`.
///
/// # Panics
///
/// Panics when GPUI has not registered its application class before setup.
pub fn install_nsapp_protocol() {
    PROTOCOL_INSTALL.call_once(|| {
        let class =
            AnyClass::get(c"GPUIApplication").expect("GPUIApplication must exist before CEF setup");
        // SAFETY: GPUI registers this NSApplication subclass in its constructor
        // before main. These method ABIs and encodings exactly match CefAppProtocol.
        #[allow(unsafe_code)]
        unsafe {
            let class_ptr = std::ptr::from_ref(class).cast_mut();
            let is_handling: Imp = std::mem::transmute::<
                unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool,
                Imp,
            >(is_handling_send_event);
            let set_handling: Imp = std::mem::transmute::<
                unsafe extern "C-unwind" fn(*mut AnyObject, Sel, Bool),
                Imp,
            >(set_handling_send_event);
            let send_event: Imp = std::mem::transmute::<
                unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
                Imp,
            >(send_event);
            ensure_method(class_ptr, sel!(isHandlingSendEvent), is_handling, c"B@:");
            ensure_method(
                class_ptr,
                sel!(setHandlingSendEvent:),
                set_handling,
                c"v@:B",
            );
            ensure_method(class_ptr, sel!(sendEvent:), send_event, c"v@:@");
            if let Some(protocol) = <dyn cef::application_mac::CefAppProtocol>::protocol() {
                let _ = ffi::class_addProtocol(class_ptr, std::ptr::from_ref(protocol));
            }
        }
    });
}

#[allow(unsafe_code)]
unsafe fn ensure_method(
    class: *mut AnyClass,
    selector: Sel,
    implementation: Imp,
    encoding: &std::ffi::CStr,
) {
    if !unsafe { ffi::class_addMethod(class, selector, implementation, encoding.as_ptr()) }
        .as_bool()
    {
        let class = unsafe { class.as_ref() }.expect("GPUIApplication class disappeared");
        assert!(
            class.instance_method(selector).is_some(),
            "failed to add {selector}"
        );
    }
}

#[allow(unsafe_code)]
unsafe extern "C-unwind" fn is_handling_send_event(_: *mut AnyObject, _: Sel) -> Bool {
    Bool::from(HANDLING_SEND_EVENT.load(Ordering::Relaxed))
}

#[allow(unsafe_code)]
unsafe extern "C-unwind" fn set_handling_send_event(_: *mut AnyObject, _: Sel, handling: Bool) {
    HANDLING_SEND_EVENT.store(handling.as_bool(), Ordering::Relaxed);
}

#[allow(unsafe_code)]
unsafe extern "C-unwind" fn send_event(this: *mut AnyObject, _: Sel, event: *mut AnyObject) {
    let was_handling = HANDLING_SEND_EVENT.swap(true, Ordering::Relaxed);
    let superclass = AnyClass::get(c"GPUIApplication")
        .and_then(AnyClass::superclass)
        .expect("GPUIApplication has no superclass");
    let mut context = ffi::objc_super {
        receiver: this,
        super_class: superclass,
    };
    let send_super = unsafe {
        std::mem::transmute::<
            Imp,
            unsafe extern "C-unwind" fn(*mut ffi::objc_super, Sel, *mut AnyObject),
        >(ffi::objc_msgSendSuper as Imp)
    };
    unsafe { send_super(&raw mut context, sel!(sendEvent:), event) };
    if !was_handling {
        HANDLING_SEND_EVENT.store(false, Ordering::Relaxed);
    }
}

fn framework_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let contents = executable.parent()?.parent()?;
    Some(contents.join("Frameworks/Chromium Embedded Framework.framework"))
}

fn helper_executable_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let contents = executable.parent()?.parent()?;
    let executable_name = executable.file_name()?.to_str()?;
    let helper_name = format!("{executable_name} Helper");
    Some(
        contents
            .join("Frameworks")
            .join(format!("{helper_name}.app"))
            .join("Contents/MacOS")
            .join(helper_name),
    )
}

fn appkit_rect(rect: BrowserRect, parent_height: f32) -> NSRect {
    let x = bounded_f64(rect.x, -f64::from(i32::MAX), f64::from(i32::MAX));
    let y = bounded_f64(rect.y, -f64::from(i32::MAX), f64::from(i32::MAX));
    let width = bounded_f64(rect.width, 0.0, f64::from(i32::MAX));
    let height = bounded_f64(rect.height, 0.0, f64::from(i32::MAX));
    let parent_height = bounded_f64(parent_height, 0.0, f64::from(i32::MAX));
    NSRect::new(
        NSPoint::new(
            x,
            (parent_height - y - height).clamp(-f64::from(i32::MAX), f64::from(i32::MAX)),
        ),
        NSSize::new(width, height),
    )
}

fn with_native_view(browser: &Browser, action: impl FnOnce(&NSView)) {
    let Some(host) = browser.host() else {
        return;
    };
    let handle = host.window_handle();
    if handle.is_null() {
        return;
    }
    // SAFETY: CEF documents a windowed browser's macOS handle as its retained
    // child NSView. The callback cannot retain the borrowed view.
    #[allow(unsafe_code)]
    unsafe {
        if let Some(view) = handle.cast::<NSView>().as_ref() {
            action(view);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserRect, appkit_rect};

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn appkit_rect_uses_bottom_left_origin() {
        let appkit = appkit_rect(
            BrowserRect {
                x: f32::NAN,
                y: 10.0,
                width: -1.0,
                height: 20.0,
            },
            100.0,
        );
        assert_close(appkit.origin.x, 0.0);
        assert_close(appkit.origin.y, 70.0);
        assert_close(appkit.size.width, 0.0);
        assert_close(appkit.size.height, 20.0);
    }
}
