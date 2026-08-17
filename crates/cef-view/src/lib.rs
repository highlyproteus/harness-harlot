//! Process-wide CEF runtime and owned native child-browser views.
//!
//! Call [`early_process_split`] at process entry, then
//! [`install_nsapp_protocol`] before GPUI creates `NSApplication`. Initialize
//! and shut down the singleton runtime once on the macOS main thread; CEF does
//! not support reinitialization after shutdown. Each
//! [`BrowserPane`] owns one asynchronously-created CEF child browser; `close`
//! and `Drop` idempotently request a forced close, including while creation is
//! pending. CEF invokes [`Callbacks`] on that same main thread.

/// A browser child-view rectangle in top-left-origin device-independent pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BrowserRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Main-thread notifications emitted by a browser pane.
pub struct Callbacks {
    pub on_address_change: Box<dyn Fn(String)>,
    pub on_title_change: Box<dyn Fn(String)>,
    pub on_favicon_change: Box<dyn Fn(Option<Vec<u8>>)>,
    pub on_loading_state: Box<dyn Fn(bool, bool, bool)>,
}

impl std::fmt::Debug for Callbacks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Callbacks").finish_non_exhaustive()
    }
}

#[cfg(all(feature = "cef", target_os = "macos"))]
mod cef_impl;

#[cfg(all(feature = "cef", target_os = "macos"))]
pub use cef_impl::{
    BrowserPane, early_process_split, init_runtime, install_nsapp_protocol, preflight,
    shutdown_runtime,
};
/// Whether this build includes the CEF implementation.
pub const fn available() -> bool {
    cfg!(all(feature = "cef", target_os = "macos"))
}

#[cfg(not(all(feature = "cef", target_os = "macos")))]
#[derive(Debug)]
pub struct BrowserPane {
    _main_thread_owned: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(not(all(feature = "cef", target_os = "macos")))]
pub fn install_nsapp_protocol() {}

#[cfg(not(all(feature = "cef", target_os = "macos")))]
pub fn early_process_split() -> Option<i32> {
    None
}

#[cfg(not(all(feature = "cef", target_os = "macos")))]
pub fn preflight() -> bool {
    false
}

#[cfg(not(all(feature = "cef", target_os = "macos")))]
pub fn init_runtime(_: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("CEF support is disabled")
}

#[cfg(not(all(feature = "cef", target_os = "macos")))]
pub fn shutdown_runtime() {}

#[cfg(not(all(feature = "cef", target_os = "macos")))]
impl BrowserPane {
    pub fn create(
        _: *mut std::ffi::c_void,
        _: BrowserRect,
        _: &str,
        _: Callbacks,
    ) -> anyhow::Result<Self> {
        anyhow::bail!("CEF support is disabled")
    }

    pub fn set_bounds(&self, _: BrowserRect, _: f32) {}
    pub fn set_visible(&self, _: bool) {}
    pub fn navigate(&self, _: &str) {}
    pub fn back(&self) {}
    pub fn forward(&self) {}
    pub fn reload(&self) {}
    pub fn stop(&self) {}
    pub fn focus(&self, _: bool) {}
    pub fn close(&self) {}
}
