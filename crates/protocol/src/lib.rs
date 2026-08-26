//! Local protocol types for Harness Harlot.
//!
//! Concerns live in focused child modules and are re-exported from this root,
//! so consumers keep importing `hh_protocol::X` directly:
//!
//! - `model`: desired-state snapshot, workspace, tab, and pane types.
//! - `profile`: stable terminal profiles and bounded detection.
//! - `terminal`: screen state, streaming cursors, and notifications.
//! - `history`: history settings, archive status, and archived pages.
//! - `messages`: request and response enums.
//! - `wire`: bounded length-prefixed JSON framing.
//! - `paths`: owner-only runtime paths and private-file access.
//! - `validation`: shared input validation.

/// Wire protocol version exchanged in the strict-equality `Hello`
/// handshake. Bump on ANY wire-visible shape change (request/response
/// variants, model serde). The persistence recovery `SCHEMA_VERSION`
/// (hh-session-service) and the history `CONFIG_SCHEMA`/`MANIFEST_SCHEMA`
/// version independently and MUST NOT force a wire bump. Because the
/// handshake is strict equality, a bump orphans every live service until
/// the desktop relaunches them.
pub const PROTOCOL_VERSION: u16 = 29;

pub const MAX_SSH_HOST_LEN: usize = 253;
pub const MAX_SSH_INPUT_LEN: usize = MAX_SSH_HOST_LEN + 16;
/// Maximum UTF-8 byte length accepted for a browser URL on the wire.
pub const MAX_BROWSER_URL_LEN: usize = 8 * 1024;
pub const DEFAULT_BROWSER_URL: &str = "about:blank";
pub const MAX_WORKSPACE_DIR_BYTES: usize = 4096;

pub const MAX_PANES: usize = 32;
pub const MIN_TERMINAL_COLUMNS: u16 = 2;
pub const MIN_TERMINAL_ROWS: u16 = 1;
pub const MAX_TERMINAL_COLUMNS: u16 = 2_048;
pub const MAX_TERMINAL_ROWS: u16 = 1_000;
pub const MAX_TERMINAL_CELLS: u32 = 600_000;
pub const MAX_UNIX_SOCKET_PATH_BYTES: usize = 103;

mod history;
mod messages;
mod model;
mod paths;
mod profile;
mod terminal;
mod validation;
mod wire;

pub use history::*;
pub use messages::*;
pub use model::*;
pub use paths::*;
pub use profile::*;
pub use terminal::*;
pub use validation::*;
pub use wire::*;
