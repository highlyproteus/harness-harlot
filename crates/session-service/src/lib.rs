#![allow(clippy::missing_errors_doc)]

//! Session registry and Unix-socket RPC service for Harness Harlot.
//!
//! [`SessionRegistry`] owns every PTY, browser, and tmux-attach runtime pane
//! plus the desired-state snapshot; [`serve_connection`] frames one
//! authenticated client connection over a Unix-domain socket.

mod history;
mod layout;
mod persistence;
mod process;
mod pty;
mod registry;
mod rpc;
mod tmux;

pub use registry::{PaneUpdateBatch, SessionRegistry, TmuxAttachmentResult, TmuxScanResult};
pub use rpc::serve_connection;
