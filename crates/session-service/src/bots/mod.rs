//! Bots: coordinator agent CLIs that run in terminals of their own bot
//! workspace, plus discovery of the coding agents they can launch.
mod discovery;
mod launch;
mod threads;

pub(crate) use discovery::discover_coding_agents;
pub(crate) use launch::{
    BotLaunch, PreparedLaunch, bot_home, bots_directory, prepare_launch, remove_bot_files,
};
pub(crate) use threads::{
    SavedThread, delete_saved_thread, saved_threads, threads_directory, valid_session_id,
};

/// Exported to a bot terminal so the Harness Harlot tools can attribute the
/// workers it opens to that bot; its value is the bot workspace id.
pub(crate) const BOT_ID_ENV: &str = "HH_BOT_ID";
