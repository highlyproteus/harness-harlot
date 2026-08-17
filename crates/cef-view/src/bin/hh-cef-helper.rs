use cef::{App, api_hash, execute_process};

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
