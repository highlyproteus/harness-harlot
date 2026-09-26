//! macOS privacy (TCC) bridge for Harness Harlot.
//!
//! macOS attributes Screen Recording and Accessibility use to a process's
//! *responsible process*: the app that started it, inherited through every
//! fork and spawn. When that app process exits, its descendants become their
//! own responsible processes and lose its permissions. Harness Harlot's
//! terminals outlive the window by design, so the desktop starts them under a
//! session host spawned with responsibility disclaimed: the host is
//! responsible for itself under the app's code identity, and everything it
//! launches keeps Harness Harlot's permissions for as long as the host lives.
#![cfg(target_os = "macos")]
#![allow(
    unsafe_code,
    reason = "this crate is the audited FFI boundary for macOS privacy and process APIs"
)]

use std::ffi::{CString, OsStr, c_char, c_int, c_void};
use std::io::{self, Read as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr;

/// `<sys/spawn.h>` `POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT`: start
/// the child in a new session and pass it only the descriptors set up by the
/// file actions.
const SPAWN_FLAGS: libc::c_short = 0x0400 | 0x4000;
/// Output captured from a status probe is a few dozen bytes.
const MAX_CAPTURED_OUTPUT: u64 = 64 * 1024;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    safe fn CGPreflightScreenCaptureAccess() -> bool;
    safe fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: [usize; 6];
    static kCFTypeDictionaryValueCallBacks: [usize; 5];
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(object: *const c_void);
    fn CFBundleGetMainBundle() -> *const c_void;
    fn CFBundleGetIdentifier(bundle: *const c_void) -> *const c_void;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> u8;
}

/// `kCFStringEncodingUTF8`.
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

// libSystem SPI used by Chromium, LLDB, and terminal emulators to launch a
// child that is its own responsible process.
unsafe extern "C" {
    fn responsibility_spawnattrs_setdisclaim(
        attributes: *mut libc::posix_spawnattr_t,
        disclaim: c_int,
    ) -> c_int;
    safe fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
    fn csops(pid: libc::pid_t, operation: u32, buffer: *mut c_void, size: usize) -> c_int;
}

/// Whether this process's responsible app may record the screen. A process
/// caches a negative answer, so ask from a fresh process after a grant.
#[must_use]
pub fn screen_recording_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Shows the system Screen Recording prompt once per app; later calls return
/// the current answer without prompting.
pub fn request_screen_recording() -> bool {
    CGRequestScreenCaptureAccess()
}

/// Whether this process's responsible app may control the computer through
/// Accessibility.
#[must_use]
pub fn accessibility_allowed() -> bool {
    // SAFETY: a null options dictionary is documented as "no options".
    unsafe { AXIsProcessTrustedWithOptions(ptr::null()) != 0 }
}

/// The running app bundle's identifier, or `None` outside an app bundle.
#[must_use]
pub fn main_bundle_identifier() -> Option<String> {
    const BUFFER_LENGTH: isize = 256;
    let mut buffer = [0 as c_char; BUFFER_LENGTH as usize];
    // SAFETY: both CF getters follow the Get rule (no ownership transfer);
    // the buffer is writable for the length passed and NUL-terminated on
    // success.
    unsafe {
        let bundle = CFBundleGetMainBundle();
        if bundle.is_null() {
            return None;
        }
        let identifier = CFBundleGetIdentifier(bundle);
        if identifier.is_null()
            || CFStringGetCString(
                identifier,
                buffer.as_mut_ptr(),
                BUFFER_LENGTH,
                CF_STRING_ENCODING_UTF8,
            ) == 0
        {
            return None;
        }
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

/// Shows the system Accessibility prompt when the app is not yet trusted.
pub fn request_accessibility() -> bool {
    // SAFETY: the dictionary holds one CF string key and the CF boolean
    // singleton, uses the standard CF-type callbacks, and is released after
    // the call that reads it.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const kCFTypeDictionaryValueCallBacks).cast(),
        );
        if options.is_null() {
            return accessibility_allowed();
        }
        let trusted = AXIsProcessTrustedWithOptions(options) != 0;
        CFRelease(options);
        trusted
    }
}

/// Starts `program` as its own responsible process in a new session with the
/// current environment. Standard input and error are `/dev/null`; standard
/// output is `stdout` or `/dev/null`; no other descriptor is inherited.
/// The caller must reap the child with [`wait_for_exit`].
///
/// # Errors
///
/// Returns an error when an argument contains NUL or the spawn fails.
pub fn spawn_disclaimed(
    program: &Path,
    arguments: &[&OsStr],
    stdout: Option<BorrowedFd<'_>>,
) -> io::Result<u32> {
    let program = c_string(program.as_os_str())?;
    let mut argument_strings = Vec::with_capacity(arguments.len() + 1);
    argument_strings.push(program.clone());
    for argument in arguments {
        argument_strings.push(c_string(argument)?);
    }
    let environment_strings = std::env::vars_os()
        .filter_map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).ok()
        })
        .collect::<Vec<_>>();
    let argv = null_terminated(&argument_strings);
    let envp = null_terminated(&environment_strings);

    let attributes = SpawnAttributes::new()?;
    let actions = FileActions::new()?;
    // SAFETY: both objects were initialized above and outlive these calls;
    // every path is a NUL-terminated literal.
    unsafe {
        check(libc::posix_spawnattr_setflags(
            attributes.as_mut_ptr(),
            SPAWN_FLAGS,
        ))?;
        check(responsibility_spawnattrs_setdisclaim(
            attributes.as_mut_ptr(),
            1,
        ))?;
        check(libc::posix_spawn_file_actions_addopen(
            actions.as_mut_ptr(),
            0,
            c"/dev/null".as_ptr(),
            libc::O_RDONLY,
            0,
        ))?;
        match stdout {
            Some(descriptor) => check(libc::posix_spawn_file_actions_adddup2(
                actions.as_mut_ptr(),
                descriptor.as_raw_fd(),
                1,
            ))?,
            None => check(libc::posix_spawn_file_actions_addopen(
                actions.as_mut_ptr(),
                1,
                c"/dev/null".as_ptr(),
                libc::O_WRONLY,
                0,
            ))?,
        }
        check(libc::posix_spawn_file_actions_addopen(
            actions.as_mut_ptr(),
            2,
            c"/dev/null".as_ptr(),
            libc::O_WRONLY,
            0,
        ))?;
    }
    let mut pid: libc::pid_t = 0;
    // SAFETY: argv and envp are NUL-terminated arrays of pointers into
    // strings that outlive the call; the spawn objects are initialized.
    check(unsafe {
        libc::posix_spawn(
            &raw mut pid,
            program.as_ptr(),
            actions.as_ptr(),
            attributes.as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
        )
    })?;
    u32::try_from(pid).map_err(|_| io::Error::other("posix_spawn returned an invalid pid"))
}

/// Runs `program` as its own responsible process and returns its standard
/// output (at most 64 KiB) once it exits successfully.
///
/// # Errors
///
/// Returns an error when the spawn or read fails or the program exits
/// unsuccessfully.
pub fn disclaimed_output(program: &Path, arguments: &[&OsStr]) -> io::Result<Vec<u8>> {
    let (reader, writer) = io::pipe()?;
    let pid = spawn_disclaimed(program, arguments, Some(writer.as_fd()))?;
    drop(writer);
    let mut output = Vec::new();
    let read = reader.take(MAX_CAPTURED_OUTPUT).read_to_end(&mut output);
    let status = wait_for_exit(pid)?;
    read?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "{} exited with {status}",
            program.display()
        )));
    }
    Ok(output)
}

/// Waits for and reaps one child process.
///
/// # Errors
///
/// Returns an error when `pid` is not a child of this process.
pub fn wait_for_exit(pid: u32) -> io::Result<ExitStatus> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::other("pid out of range"))?;
    let mut status = 0;
    loop {
        // SAFETY: `status` is a valid out-pointer for the duration of the call.
        if unsafe { libc::waitpid(pid, &raw mut status, 0) } == pid {
            return Ok(ExitStatus::from_raw(status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The process macOS holds responsible for `pid`'s privacy-sensitive
/// requests, or `None` when `pid` does not exist.
#[must_use]
pub fn responsible_pid(pid: u32) -> Option<u32> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    u32::try_from(responsibility_get_pid_responsible_for_pid(pid))
        .ok()
        .filter(|responsible| *responsible > 0)
}

/// Every other live process whose responsible process is `pid`.
///
/// # Errors
///
/// Returns an error when the process table cannot be listed.
pub fn processes_responsible_to(pid: u32) -> io::Result<Vec<u32>> {
    Ok(all_pids()?
        .into_iter()
        .filter(|candidate| *candidate != pid && responsible_pid(*candidate) == Some(pid))
        .collect())
}

/// The code directory hash of a live process's signed code: the identity
/// macOS privacy grants follow for ad-hoc-signed builds.
#[must_use]
pub fn code_hash(pid: u32) -> Option<[u8; 20]> {
    /// `<sys/codesign.h>` `CS_OPS_CDHASH`.
    const CS_OPS_CDHASH: u32 = 5;
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut hash = [0_u8; 20];
    // SAFETY: the buffer is writable for the length passed.
    let result = unsafe { csops(pid, CS_OPS_CDHASH, hash.as_mut_ptr().cast(), hash.len()) };
    (result == 0).then_some(hash)
}

/// The process on the other end of a connected Unix-domain socket.
///
/// # Errors
///
/// Returns an error when the socket has no peer.
pub fn peer_pid(socket: BorrowedFd<'_>) -> io::Result<u32> {
    let mut pid: libc::pid_t = 0;
    let mut length = libc::socklen_t::try_from(size_of::<libc::pid_t>())
        .map_err(|_| io::Error::other("pid size overflow"))?;
    // SAFETY: `pid` and `length` are valid out-pointers sized for
    // LOCAL_PEERPID's `pid_t` result.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(pid).map_err(|_| io::Error::other("socket peer has no pid"))
}

fn all_pids() -> io::Result<Vec<u32>> {
    // SAFETY: a null buffer asks only for the current process count.
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
    // Leave room for processes started between the two calls.
    let mut pids = vec![0 as libc::pid_t; count + 64];
    let bytes = c_int::try_from(pids.len() * size_of::<libc::pid_t>())
        .map_err(|_| io::Error::other("process table too large"))?;
    // SAFETY: the buffer is writable for `bytes` bytes.
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let filled = usize::try_from(filled).map_err(|_| io::Error::last_os_error())?;
    pids.truncate(filled.min(pids.len()));
    Ok(pids
        .into_iter()
        .filter_map(|pid| u32::try_from(pid).ok().filter(|pid| *pid > 0))
        .collect())
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL"))
}

fn null_terminated(strings: &[CString]) -> Vec<*mut c_char> {
    strings
        .iter()
        .map(|string| string.as_ptr().cast_mut())
        .chain(std::iter::once(ptr::null_mut()))
        .collect()
}

fn check(result: c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

struct SpawnAttributes(libc::posix_spawnattr_t);

impl SpawnAttributes {
    fn new() -> io::Result<Self> {
        let mut attributes: libc::posix_spawnattr_t = ptr::null_mut();
        // SAFETY: `attributes` is a valid out-pointer.
        check(unsafe { libc::posix_spawnattr_init(&raw mut attributes) })?;
        Ok(Self(attributes))
    }

    fn as_ptr(&self) -> *const libc::posix_spawnattr_t {
        &raw const self.0
    }

    fn as_mut_ptr(&self) -> *mut libc::posix_spawnattr_t {
        (&raw const self.0).cast_mut()
    }
}

impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: initialized in `new` and destroyed exactly once.
        unsafe {
            libc::posix_spawnattr_destroy(&raw mut self.0);
        }
    }
}

struct FileActions(libc::posix_spawn_file_actions_t);

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
        // SAFETY: `actions` is a valid out-pointer.
        check(unsafe { libc::posix_spawn_file_actions_init(&raw mut actions) })?;
        Ok(Self(actions))
    }

    fn as_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
        &raw const self.0
    }

    fn as_mut_ptr(&self) -> *mut libc::posix_spawn_file_actions_t {
        (&raw const self.0).cast_mut()
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: initialized in `new` and destroyed exactly once.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&raw mut self.0);
        }
    }
}
