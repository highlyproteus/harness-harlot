// The `cef` wrapper macros generate pointer transmutes internally; call sites
// cannot replace those macro-owned conversions.
#![allow(clippy::transmute_ptr_to_ptr)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::{Rc as StdRc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, ensure};
use cef::*;
use dispatch2::{DispatchQueue, DispatchTime};
use image::ImageEncoder as _;
use objc2::ffi;
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{MainThreadMarker, ProtocolType as _, sel};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::{BrowserRect, Callbacks};

static PROTOCOL_INSTALL: Once = Once::new();
static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static TERMINATED: AtomicBool = AtomicBool::new(false);
static LIBRARY: OnceLock<library_loader::LibraryLoader> = OnceLock::new();

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
    parent_view: *mut c_void,
    pending_bounds: Option<NSRect>,
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
            let (parent_view, bounds, pending_url, visible, focused, close_requested) = {
                let mut state = self.state.borrow_mut();
                state.creation_pending = false;
                state.browser = Some(browser.clone());
                (
                    state.parent_view,
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
            with_native_view(&browser, |view| {
                if let Some(bounds) = bounds {
                    view.setFrame(bounds);
                }
                view.setHidden(!visible);
            });
            if let Some(url) = pending_url
                && let Some(frame) = browser.main_frame()
            {
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
            if let Some(host) = browser.host() {
                host.set_focus(i32::from(focused));
            }
            if !focused {
                restore_parent_focus(parent_view);
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
    pub fn create(
        parent_view: *mut c_void,
        rect: BrowserRect,
        url: &str,
        callbacks: Callbacks,
    ) -> anyhow::Result<Self> {
        ensure!(
            MainThreadMarker::new().is_some(),
            "browser panes must be created on the macOS main thread"
        );
        ensure!(
            INITIALIZED.load(Ordering::Acquire),
            "CEF runtime is not initialized"
        );
        ensure!(!parent_view.is_null(), "browser parent NSView is null");
        let state = StdRc::new(RefCell::new(BrowserState {
            browser: None,
            creation_pending: true,
            parent_view,
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
        let bounds = cef_rect(rect);
        let window_info = WindowInfo::default().set_as_child(parent_view.cast(), &bounds);
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
        let bounds = appkit_rect(rect, parent_height);
        let browser = {
            let mut state = self.state.borrow_mut();
            state.pending_bounds = Some(bounds);
            state.browser.clone()
        };
        if let Some(browser) = browser {
            with_native_view(&browser, |view| view.setFrame(bounds));
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
            with_native_view(&browser, |view| view.setHidden(!visible));
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
        let (browser, parent_view) = {
            let mut state = self.state.borrow_mut();
            state.presentation.focused = focused;
            (state.browser.clone(), state.parent_view)
        };
        if let Some(browser) = browser
            && let Some(host) = browser.host()
        {
            host.set_focus(i32::from(focused));
        }
        if !focused {
            restore_parent_focus(parent_view);
        }
    }

    pub fn close(&self) {
        let (browser, parent_view) = {
            let mut state = self.state.borrow_mut();
            if state.presentation.close_requested {
                return;
            }
            state.presentation.close_requested = true;
            state.presentation.visible = false;
            (state.browser.clone(), state.parent_view)
        };
        if let Some(browser) = browser {
            with_native_view(&browser, |view| view.setHidden(true));
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        }
        restore_parent_focus(parent_view);
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

fn restore_parent_focus(parent_view: *mut c_void) {
    if parent_view.is_null() {
        return;
    }
    // SAFETY: The parent GPUI NSView and its NSWindow outlive every browser pane.
    #[allow(unsafe_code)]
    unsafe {
        let parent = parent_view.cast::<AnyObject>();
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
    ensure!(
        MainThreadMarker::new().is_some(),
        "CEF runtime must be initialized on the macOS main thread"
    );
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
    let args = cef::args::Args::new();
    let settings = Settings {
        external_message_pump: 1,
        root_cache_path: CefString::from(cache_dir.to_string_lossy().as_ref()),
        ..Default::default()
    };
    let mut app = HhCefApp::new();
    ensure!(
        initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        ) != 0,
        "CEF initialization failed"
    );
    CEF_APP.with(|stored| *stored.borrow_mut() = Some(app));
    INITIALIZED.store(true, Ordering::Release);
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

/// Closes every live child browser and shuts CEF down when close callbacks drain.
///
/// `AppKit` can begin termination from inside CEF's external message-pump callback.
/// Re-entering that pump may then be unable to deliver close callbacks, so the
/// drain is bounded. On timeout the process is already terminating; retaining
/// CEF's state for process teardown is safer than calling `cef_shutdown` with a
/// live browser or hanging application termination indefinitely.
pub fn shutdown_runtime() {
    if MainThreadMarker::new().is_none() || !INITIALIZED.load(Ordering::Acquire) {
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

fn cef_rect(rect: BrowserRect) -> Rect {
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

fn bounded_f64(value: f32, minimum: f64, maximum: f64) -> f64 {
    if value.is_finite() {
        f64::from(value).clamp(minimum, maximum)
    } else {
        0.0
    }
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
    use super::{BrowserRect, appkit_rect, cef_rect};
    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn rect_conversion_sanitizes_invalid_and_empty_values() {
        let cef = cef_rect(BrowserRect {
            x: f32::NAN,
            y: f32::INFINITY,
            width: -20.0,
            height: 0.0,
        });
        assert_eq!((cef.x, cef.y, cef.width, cef.height), (0, 0, 1, 1));

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
