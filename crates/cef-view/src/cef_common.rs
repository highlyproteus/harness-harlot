// The `cef` wrapper macros generate pointer transmutes internally; call sites
// cannot replace those macro-owned conversions.
#![allow(clippy::transmute_ptr_to_ptr)]

use std::cell::RefCell;
use std::rc::{Rc as StdRc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::ensure;
use cef::*;
use image::ImageEncoder as _;

use super::{BrowserRect, Callbacks};
#[cfg(target_os = "linux")]
use crate::cef_linux as platform;
#[cfg(target_os = "macos")]
use crate::cef_macos as platform;

pub(crate) static INITIALIZED: AtomicBool = AtomicBool::new(false);
pub(crate) static TERMINATED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static CEF_APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static BROWSERS: RefCell<Vec<Weak<RefCell<BrowserState>>>> = const { RefCell::new(Vec::new()) };
}

wrap_app! {
    struct HhCefApp;

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(HhBrowserProcessHandler::new())
        }

        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            // CEF otherwise requests the process-global "Chromium Safe Storage"
            // keychain item, which belongs to whichever unrelated Chromium
            // embedder created it first. Avoid that cross-app prompt; the
            // browser profile remains confined to Harness Harlot's owner-only
            // state directory.
            if let Some(command_line) = command_line {
                command_line.append_switch(Some(&CefString::from("use-mock-keychain")));
            }
        }
    }
}
wrap_browser_process_handler! {
    struct HhBrowserProcessHandler;

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, _delay_ms: i64) {
            // A single dispatch timer below drives CEF from the GPUI run loop.
            // Keeping scheduling in one place prevents overlapping pump calls.
        }
    }
}

struct BrowserPresentation {
    visible: bool,
    focused: bool,
    close_requested: bool,
}

struct BrowserState {
    browser: Option<Browser>,
    creation_pending: bool,
    parent: platform::ParentHandle,
    pending_bounds: Option<(BrowserRect, f32)>,
    pending_url: Option<String>,
    presentation: BrowserPresentation,
    favicon_url: Option<String>,
}

wrap_client! {
    struct HhBrowserClient {
        state: StdRc<RefCell<BrowserState>>,
        callbacks: StdRc<Callbacks>,
    }

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(HhDisplayHandler::new(
                self.state.clone(),
                self.callbacks.clone(),
            ))
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(HhLifeSpanHandler::new(self.state.clone()))
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(HhLoadHandler::new(self.callbacks.clone()))
        }
    }
}

wrap_display_handler! {
    struct HhDisplayHandler {
        state: StdRc<RefCell<BrowserState>>,
        callbacks: StdRc<Callbacks>,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if frame.is_some_and(|frame| frame.is_main() == 0) {
                return;
            }
            let url = url.map(CefString::to_string).unwrap_or_default();
            (self.callbacks.on_address_change)(url);
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            let title = title.map(CefString::to_string).unwrap_or_default();
            (self.callbacks.on_title_change)(title);
        }

        fn on_favicon_urlchange(
            &self,
            browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            let icon_url = icon_urls
                .and_then(|urls| std::mem::take(urls).into_iter().next())
                .filter(|url| !url.is_empty());
            {
                let mut state = self.state.borrow_mut();
                if state.favicon_url == icon_url {
                    return;
                }
                state.favicon_url.clone_from(&icon_url);
            }
            let Some(icon_url) = icon_url else {
                (self.callbacks.on_favicon_change)(None);
                return;
            };
            let Some(host) = browser.and_then(|browser| browser.host()) else {
                return;
            };
            let mut callback = HhFaviconDownload::new(
                self.state.clone(),
                self.callbacks.clone(),
                icon_url.clone(),
            );
            host.download_image(
                Some(&CefString::from(icon_url.as_str())),
                1,
                32,
                0,
                Some(&mut callback),
            );
        }
    }
}

fn binary_value_bytes(data: &BinaryValue, max_size: usize) -> Option<Vec<u8>> {
    let size = data.size();
    if size == 0 || size > max_size {
        return None;
    }
    let mut bytes = vec![0; size];
    let written = data.data(Some(&mut bytes), 0);
    if written == 0 || written > size {
        return None;
    }
    bytes.truncate(written);
    Some(bytes)
}

fn favicon_png(image: &Image) -> Option<Vec<u8>> {
    if let Some(png) = image
        .as_png(1.0, 1, None, None)
        .and_then(|data| binary_value_bytes(&data, 1_048_576))
    {
        return Some(png);
    }

    let mut width = 0;
    let mut height = 0;
    let rgba = image
        .as_bitmap(
            1.0,
            ColorType::RGBA_8888,
            AlphaType::POSTMULTIPLIED,
            Some(&mut width),
            Some(&mut height),
        )
        .and_then(|data| binary_value_bytes(&data, 1_048_576))?;
    let width = u32::try_from(width).ok().filter(|width| *width > 0)?;
    let height = u32::try_from(height).ok().filter(|height| *height > 0)?;
    let expected_size = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    if rgba.len() != expected_size {
        return None;
    }

    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(png)
}

wrap_download_image_callback! {
    struct HhFaviconDownload {
        state: StdRc<RefCell<BrowserState>>,
        callbacks: StdRc<Callbacks>,
        icon_url: String,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(
            &self,
            _image_url: Option<&CefString>,
            _http_status_code: i32,
            image: Option<&mut Image>,
        ) {
            if self.state.borrow().favicon_url.as_deref() != Some(self.icon_url.as_str()) {
                return;
            }
            (self.callbacks.on_favicon_change)(image.as_deref().and_then(favicon_png));
        }

    }
}

wrap_life_span_handler! {
    struct HhLifeSpanHandler {
        state: StdRc<RefCell<BrowserState>>,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            if let Some(url) = target_url
                && let Some(frame) = browser.and_then(|browser| browser.main_frame())
            {
                frame.load_url(Some(url));
            }
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser.cloned() else {
                return;
            };
            let (parent, bounds, pending_url, visible, focused, close_requested) = {
                let mut state = self.state.borrow_mut();
                state.creation_pending = false;
                state.browser = Some(browser.clone());
                (
                    state.parent,
                    state.pending_bounds.take(),
                    state.pending_url.take(),
                    state.presentation.visible,
                    state.presentation.focused,
                    state.presentation.close_requested,
                )
            };
            if close_requested {
                if let Some(host) = browser.host() {
                    host.close_browser(1);
                }
                return;
            }
            if let Some((rect, parent_height)) = bounds {
                platform::apply_bounds(&browser, rect, parent_height);
            }
            platform::set_visible(&browser, visible);
            if let Some(url) = pending_url
                && let Some(frame) = browser.main_frame()
            {
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
            if let Some(host) = browser.host() {
                host.set_focus(i32::from(focused));
            }
            if !focused {
                platform::focus_parent(parent);
            }
        }
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            1
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            let mut state = self.state.borrow_mut();
            state.creation_pending = false;
            state.browser = None;
        }
    }
}

wrap_load_handler! {
    struct HhLoadHandler {
        callbacks: StdRc<Callbacks>,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            can_go_back: i32,
            can_go_forward: i32,
        ) {
            (self.callbacks.on_loading_state)(
                is_loading != 0,
                can_go_back != 0,
                can_go_forward != 0,
            );
        }
    }
}

/// Main-thread owner of one windowed CEF child browser.
///
/// Creation completes asynchronously. Mutations issued before
/// `on_after_created` are retained and applied there; `close` is idempotent and
/// dropping the owner follows the same forced-close path.
pub struct BrowserPane {
    state: StdRc<RefCell<BrowserState>>,
}
impl std::fmt::Debug for BrowserPane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserPane")
            .field("created", &self.state.borrow().browser.is_some())
            .finish_non_exhaustive()
    }
}

impl BrowserPane {
    /// Creates a child browser attached to the supplied parent view.
    ///
    /// # Errors
    ///
    /// Returns an error when called off the main thread, before CEF
    /// initialization, with a null parent view, or when CEF rejects creation.
    #[cfg(target_os = "macos")]
    pub fn create(
        parent: platform::ParentHandle,
        rect: BrowserRect,
        url: &str,
        callbacks: Callbacks,
    ) -> anyhow::Result<Self> {
        ensure!(!parent.is_null(), "browser parent NSView is null");
        Self::create_with_parent(parent, rect, url, callbacks)
    }

    /// Creates a child browser attached to the application's X11 window.
    ///
    /// # Errors
    ///
    /// Returns an error before CEF initialization, when the parent X11 window
    /// cannot be found, or when CEF rejects creation.
    #[cfg(target_os = "linux")]
    pub fn create(rect: BrowserRect, url: &str, callbacks: Callbacks) -> anyhow::Result<Self> {
        let parent = platform::find_parent_window()?;
        Self::create_with_parent(parent, rect, url, callbacks)
    }

    fn create_with_parent(
        parent: platform::ParentHandle,
        rect: BrowserRect,
        url: &str,
        callbacks: Callbacks,
    ) -> anyhow::Result<Self> {
        platform::ensure_main_thread()?;
        ensure!(
            INITIALIZED.load(Ordering::Acquire),
            "CEF runtime is not initialized"
        );
        let state = StdRc::new(RefCell::new(BrowserState {
            browser: None,
            creation_pending: true,
            parent,
            pending_bounds: None,
            pending_url: None,
            presentation: BrowserPresentation {
                visible: true,
                focused: false,
                close_requested: false,
            },
            favicon_url: None,
        }));
        let callbacks = StdRc::new(callbacks);
        let mut client = HhBrowserClient::new(state.clone(), callbacks);
        let window_info = platform::create_window_info(parent, rect);
        let created = browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&BrowserSettings::default()),
            None,
            None,
        );
        ensure!(created != 0, "CEF rejected browser creation");
        BROWSERS.with(|browsers| {
            let mut browsers = browsers.borrow_mut();
            browsers.retain(|browser| browser.strong_count() > 0);
            browsers.push(StdRc::downgrade(&state));
        });
        Ok(Self { state })
    }

    pub fn set_bounds(&self, rect: BrowserRect, parent_height: f32) {
        let browser = {
            let mut state = self.state.borrow_mut();
            state.pending_bounds = Some((rect, parent_height));
            state.browser.clone()
        };
        if let Some(browser) = browser {
            platform::apply_bounds(&browser, rect, parent_height);
        }
    }

    pub fn set_visible(&self, visible: bool) {
        let browser = {
            let mut state = self.state.borrow_mut();
            if state.presentation.visible == visible {
                return;
            }
            state.presentation.visible = visible;
            state.browser.clone()
        };
        if let Some(browser) = browser {
            platform::set_visible(&browser, visible);
        }
    }

    pub fn navigate(&self, url: &str) {
        let browser = {
            let mut state = self.state.borrow_mut();
            if state.presentation.close_requested {
                return;
            }
            let browser = state.browser.clone();
            if browser.is_none() {
                state.pending_url = Some(url.to_owned());
            }
            browser
        };
        if let Some(browser) = browser
            && let Some(frame) = browser.main_frame()
        {
            frame.load_url(Some(&CefString::from(url)));
        }
    }

    pub fn back(&self) {
        self.with_browser(cef::ImplBrowser::go_back);
    }

    pub fn forward(&self) {
        self.with_browser(cef::ImplBrowser::go_forward);
    }

    pub fn reload(&self) {
        self.with_browser(cef::ImplBrowser::reload);
    }

    pub fn stop(&self) {
        self.with_browser(cef::ImplBrowser::stop_load);
    }

    pub fn focus(&self, focused: bool) {
        let (browser, parent) = {
            let mut state = self.state.borrow_mut();
            state.presentation.focused = focused;
            (state.browser.clone(), state.parent)
        };
        if let Some(browser) = browser
            && let Some(host) = browser.host()
        {
            host.set_focus(i32::from(focused));
        }
        if !focused {
            platform::focus_parent(parent);
        }
    }

    pub fn close(&self) {
        let (browser, parent) = {
            let mut state = self.state.borrow_mut();
            if state.presentation.close_requested {
                return;
            }
            state.presentation.close_requested = true;
            state.presentation.visible = false;
            (state.browser.clone(), state.parent)
        };
        if let Some(browser) = browser {
            platform::set_visible(&browser, false);
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        }
        platform::focus_parent(parent);
    }

    fn with_browser(&self, action: impl FnOnce(&Browser)) {
        let browser = {
            let state = self.state.borrow();
            (!state.presentation.close_requested)
                .then(|| state.browser.clone())
                .flatten()
        };
        if let Some(browser) = browser.as_ref() {
            action(browser);
        }
    }
}

impl Drop for BrowserPane {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) fn initialize_with_settings(settings: &Settings) -> anyhow::Result<()> {
    let args = cef::args::Args::new();
    let mut app = HhCefApp::new();
    ensure!(
        initialize(
            Some(args.as_main_args()),
            Some(settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ) != 0,
        "CEF initialization failed"
    );
    CEF_APP.with(|stored| *stored.borrow_mut() = Some(app));
    INITIALIZED.store(true, Ordering::Release);
    Ok(())
}
#[cfg(target_os = "linux")]
pub fn pump_runtime() {
    if INITIALIZED.load(Ordering::Acquire) && platform::ensure_main_thread().is_ok() {
        do_message_loop_work();
    }
}

/// Closes every live child browser and shuts CEF down when close callbacks drain.
///
/// `AppKit` can begin termination from inside CEF's external message-pump callback.
/// Re-entering that pump may then be unable to deliver close callbacks, so the
/// drain is bounded. On timeout the process is already terminating; retaining
/// CEF's state for process teardown is safer than calling `cef_shutdown` with a
/// live browser or hanging application termination indefinitely.
pub fn shutdown_runtime() {
    if platform::ensure_main_thread().is_err() || !INITIALIZED.load(Ordering::Acquire) {
        return;
    }
    BROWSERS.with(|browsers| {
        for state in browsers.borrow().iter().filter_map(Weak::upgrade) {
            let browser = {
                let mut state = state.borrow_mut();
                state.presentation.close_requested = true;
                state.browser.clone()
            };
            if let Some(browser) = browser
                && let Some(host) = browser.host()
            {
                host.close_browser(1);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let drained = loop {
        let browsers_pending = BROWSERS.with(|browsers| {
            let mut browsers = browsers.borrow_mut();
            browsers.retain(|state| state.strong_count() > 0);
            browsers.iter().filter_map(Weak::upgrade).any(|state| {
                let state = state.borrow();
                state.creation_pending || state.browser.is_some()
            })
        });
        if !browsers_pending {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(1));
    };
    INITIALIZED.store(false, Ordering::Release);
    if drained {
        shutdown();
        CEF_APP.with(|app| *app.borrow_mut() = None);
    } else {
        eprintln!("CEF browser close callbacks did not drain before application termination");
    }
    TERMINATED.store(true, Ordering::Release);
}

pub(crate) fn cef_rect(rect: BrowserRect) -> Rect {
    Rect {
        x: cef_coordinate(rect.x),
        y: cef_coordinate(rect.y),
        width: cef_extent(rect.width),
        height: cef_extent(rect.height),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn rounded_clamped_i32(value: f32, minimum: i32, fallback: i32) -> i32 {
    if !value.is_finite() {
        return fallback;
    }
    f64::from(value)
        .round()
        .clamp(f64::from(minimum), f64::from(i32::MAX)) as i32
}

fn cef_coordinate(value: f32) -> i32 {
    rounded_clamped_i32(value, i32::MIN, 0)
}

fn cef_extent(value: f32) -> i32 {
    rounded_clamped_i32(value, 1, 1)
}

#[cfg(target_os = "macos")]
pub(crate) fn bounded_f64(value: f32, minimum: f64, maximum: f64) -> f64 {
    if value.is_finite() {
        f64::from(value).clamp(minimum, maximum)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowserRect, cef_rect};

    #[test]
    fn rect_conversion_sanitizes_invalid_and_empty_values() {
        let cef = cef_rect(BrowserRect {
            x: f32::NAN,
            y: f32::INFINITY,
            width: -20.0,
            height: 0.0,
        });
        assert_eq!((cef.x, cef.y, cef.width, cef.height), (0, 0, 1, 1));
    }
}
