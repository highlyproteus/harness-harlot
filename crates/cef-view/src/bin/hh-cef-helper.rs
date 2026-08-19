// The `cef` wrapper macro generates pointer transmutes internally; call sites
// cannot replace those macro-owned conversions.
#![allow(clippy::transmute_ptr_to_ptr)]

#[cfg(all(target_os = "linux", feature = "cef"))]
use cef::rc::Rc as _;
#[cfg(all(target_os = "linux", feature = "cef"))]
use cef::{App, ImplApp, WrapApp, api_hash, execute_process, wrap_app};
#[cfg(target_os = "macos")]
use cef::{App, api_hash, execute_process};

#[cfg(all(target_os = "linux", feature = "cef"))]
wrap_app! {
    struct LinuxHelperApp;

    impl App {}
}

#[cfg(target_os = "macos")]
fn main() {
    let args = cef::args::Args::new();

    #[cfg(target_os = "macos")]
    let _sandbox = {
        let mut sandbox = cef::sandbox::Sandbox::new();
        sandbox.initialize(args.as_main_args());
        sandbox
    };

    #[cfg(target_os = "macos")]
    let _loader = {
        let Ok(executable) = std::env::current_exe() else {
            std::process::exit(1);
        };
        let loader = cef::library_loader::LibraryLoader::new(&executable, true);
        if !loader.load() {
            std::process::exit(1);
        }
        loader
    };

    if api_hash(cef::sys::CEF_API_VERSION_LAST, 0).is_null() {
        std::process::exit(1);
    }

    let code = execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
    std::process::exit(if code >= 0 { code } else { 1 });
}

#[cfg(all(target_os = "linux", feature = "cef"))]
fn main() {
    let args = cef::args::Args::new();
    if api_hash(cef::sys::CEF_API_VERSION_LAST, 0).is_null() {
        std::process::exit(1);
    }

    let mut app = LinuxHelperApp::new();
    let code = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    std::process::exit(if code >= 0 { code } else { 1 });
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", feature = "cef"))))]
fn main() {}
