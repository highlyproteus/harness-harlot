#![allow(clippy::missing_errors_doc)]

//! Session registry and Unix-socket RPC service for Harness Harlot.
//!
//! [`SessionRegistry`] owns every PTY, browser, and tmux-attach runtime pane
//! plus the desired-state snapshot; [`serve_connection`] frames one
//! authenticated client connection over a Unix-domain socket.

mod bots;
mod gallery;
mod layout;
mod paste_events;
mod persistence;
mod process;
mod pty;
mod registry;
mod rpc;
mod terminal_images;
mod tmux;
mod tmux_control;

pub use registry::{PaneUpdateBatch, SessionRegistry, TmuxAttachmentResult, TmuxScanResult};
pub use rpc::serve_connection;
pub use terminal_images::clear_stale_terminal_images;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
