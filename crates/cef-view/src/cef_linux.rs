use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::thread::ThreadId;

use anyhow::{Context as _, ensure};
use cef::{Browser, CefString, ImplBrowser as _, ImplBrowserHost as _, Settings, WindowInfo};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt as _, InputFocus, Window,
};
use x11rb::rust_connection::RustConnection;

use super::BrowserRect;
use crate::cef_common::{self, INITIALIZED, TERMINATED, cef_rect};

pub(crate) type ParentHandle = u32;

struct X11State {
    connection: RustConnection,
    screen_number: usize,
    net_wm_pid: Atom,
    net_client_list: Atom,
}

static X11: OnceLock<X11State> = OnceLock::new();
static RUNTIME_THREAD: OnceLock<ThreadId> = OnceLock::new();

pub(crate) fn ensure_main_thread() -> anyhow::Result<()> {
    let expected = RUNTIME_THREAD
        .get()
        .context("CEF runtime thread is not initialized")?;
    ensure!(
        std::thread::current().id() == *expected,
        "CEF runtime must run on its initializing thread"
    );
    Ok(())
}

pub(crate) fn create_window_info(parent: ParentHandle, rect: BrowserRect) -> WindowInfo {
    WindowInfo::default().set_as_child(u64::from(parent), &cef_rect(rect))
}

pub(crate) fn apply_bounds(browser: &Browser, rect: BrowserRect, _parent_height: f32) {
    let Some(window) = browser_window(browser) else {
        return;
    };
    let bounds = cef_rect(rect);
    let Ok(width) = u32::try_from(bounds.width) else {
        return;
    };
    let Ok(height) = u32::try_from(bounds.height) else {
        return;
    };
    if let Ok(x11) = x11_state() {
        let values = ConfigureWindowAux::new()
            .x(bounds.x)
            .y(bounds.y)
            .width(width)
            .height(height);
        let _ = x11.connection.configure_window(window, &values);
        let _ = x11.connection.flush();
    }
}

pub(crate) fn set_visible(browser: &Browser, visible: bool) {
    let Some(window) = browser_window(browser) else {
        return;
    };
    if let Ok(x11) = x11_state() {
        if visible {
            let _ = x11.connection.map_window(window);
        } else {
            let _ = x11.connection.unmap_window(window);
        }
        let _ = x11.connection.flush();
    }
}

pub(crate) fn focus_parent(parent: ParentHandle) {
    if let Ok(x11) = x11_state() {
        let _ = x11
            .connection
            .set_input_focus(InputFocus::PARENT, parent, CURRENT_TIME);
        let _ = x11.connection.flush();
    }
}

fn browser_window(browser: &Browser) -> Option<Window> {
    let host = browser.host()?;
    u32::try_from(host.window_handle())
        .ok()
        .filter(|window| *window != 0)
}

/// Returns whether the packaged CEF runtime is present beside this executable.
pub fn preflight() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
        .is_some_and(|directory| directory.join("icudtl.dat").is_file())
}

/// Linux CEF subprocesses use the separately packaged `hh-cef-helper` binary.
pub const fn early_process_split() -> Option<i32> {
    None
}

/// Linux does not require the macOS application protocol integration.
pub const fn install_nsapp_protocol() {}

/// Returns whether Chromium can use the unprivileged user-namespace sandbox.
pub fn sandbox_available() -> bool {
    let apparmor =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").ok();
    let userns_clone = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone").ok();
    userns_allowed(apparmor.as_deref(), userns_clone.as_deref())
}

fn userns_allowed(apparmor: Option<&str>, userns_clone: Option<&str>) -> bool {
    apparmor.is_none_or(|value| value.trim() != "1")
        && userns_clone.is_none_or(|value| value.trim() != "0")
}

/// Initializes the process-wide external-message-pump runtime on the main thread.
///
/// # Errors
///
/// Returns an error after shutdown, when the packaged runtime or helper is
/// absent, when the kernel restricts user namespaces, or when CEF fails to
/// initialize.
pub fn init_runtime(cache_dir: &Path) -> anyhow::Result<()> {
    let current_thread = std::thread::current().id();
    if let Some(runtime_thread) = RUNTIME_THREAD.get() {
        ensure!(
            *runtime_thread == current_thread,
            "CEF runtime must run on its initializing thread"
        );
    } else {
        let _ = RUNTIME_THREAD.set(current_thread);
    }
    ensure!(
        !TERMINATED.load(Ordering::Acquire),
        "CEF runtime cannot be initialized again after shutdown"
    );
    if INITIALIZED.load(Ordering::Acquire) {
        return Ok(());
    }
    ensure!(preflight(), "CEF runtime is absent beside the executable");
    ensure!(
        sandbox_available(),
        "browser tabs require unprivileged user namespaces; this kernel restricts them"
    );
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("create browser cache directory {}", cache_dir.display()))?;
    let cache_dir = cache_dir
        .canonicalize()
        .with_context(|| format!("resolve browser cache directory {}", cache_dir.display()))?;
    let executable = std::env::current_exe().context("resolve current executable")?;
    let executable_dir = executable
        .parent()
        .context("resolve executable directory")?;
    let subprocess_path = executable_dir.join("hh-cef-helper");
    ensure!(
        subprocess_path.is_file(),
        "CEF helper executable is absent at {}",
        subprocess_path.display()
    );
    let settings = Settings {
        browser_subprocess_path: CefString::from(subprocess_path.to_string_lossy().as_ref()),
        cache_path: CefString::from(cache_dir.to_string_lossy().as_ref()),
        root_cache_path: CefString::from(cache_dir.to_string_lossy().as_ref()),
        external_message_pump: 1,
        ..Default::default()
    };
    cef_common::initialize_with_settings(&settings)
}

pub(crate) fn find_parent_window() -> anyhow::Result<ParentHandle> {
    let x11 = x11_state()?;
    let root = x11.connection.setup().roots[x11.screen_number].root;
    let clients = x11
        .connection
        .get_property(
            false,
            root,
            x11.net_client_list,
            AtomEnum::WINDOW,
            0,
            u32::MAX,
        )?
        .reply()?;
    if let Some(window) = clients
        .value32()
        .into_iter()
        .flatten()
        .find(|window| window_matches_pid(x11, *window))
    {
        return Ok(window);
    }

    let tree = x11.connection.query_tree(root)?.reply()?;
    tree.children
        .into_iter()
        .find(|window| window_matches_pid(x11, *window))
        .context("could not find the application's top-level X11 window")
}

fn window_matches_pid(x11: &X11State, window: Window) -> bool {
    x11.connection
        .get_property(false, window, x11.net_wm_pid, AtomEnum::CARDINAL, 0, 1)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|property| property.value32().and_then(|mut values| values.next()))
        == Some(std::process::id())
}

fn x11_state() -> anyhow::Result<&'static X11State> {
    if let Some(x11) = X11.get() {
        return Ok(x11);
    }
    let (connection, screen_number) = x11rb::connect(None).context("connect to X11 display")?;
    let net_wm_pid = connection.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    let net_client_list = connection
        .intern_atom(false, b"_NET_CLIENT_LIST")?
        .reply()?
        .atom;
    let _ = X11.set(X11State {
        connection,
        screen_number,
        net_wm_pid,
        net_client_list,
    });
    X11.get().context("initialize X11 connection")
}

#[cfg(test)]
mod tests {
    use super::userns_allowed;

    #[test]
    fn apparmor_restriction_disables_user_namespaces() {
        assert!(!userns_allowed(Some("1\n"), Some("1\n")));
    }

    #[test]
    fn clone_restriction_disables_user_namespaces() {
        assert!(!userns_allowed(Some("0\n"), Some("0\n")));
    }

    #[test]
    fn missing_kernel_gates_allow_user_namespaces() {
        assert!(userns_allowed(None, None));
    }

    #[test]
    fn explicit_allow_values_allow_user_namespaces() {
        assert!(userns_allowed(Some("0\n"), Some("1\n")));
    }
}
