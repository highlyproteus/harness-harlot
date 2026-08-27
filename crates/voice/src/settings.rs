use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const SETTINGS_SCHEMA_VERSION: u32 = 1;
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const SETTINGS_FILE: &str = "voice-settings.json";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceSettings {
    pub schema_version: u32,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub full_duplex: bool,
    pub idle_timeout_secs: u32,
    pub honcho: Option<HonchoSettings>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HonchoSettings {
    pub base_url: String,
    pub workspace: String,
    pub bearer: Option<String>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            api_key: String::new(),
            model: "gpt-realtime-2.1".to_owned(),
            voice: "marin".to_owned(),
            full_duplex: false,
            idle_timeout_secs: 900,
            honcho: None,
        }
    }
}

impl Default for HonchoSettings {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8000".to_owned(),
            workspace: "harness-harlot".to_owned(),
            bearer: None,
        }
    }
}

impl VoiceSettings {
    #[must_use]
    pub fn load() -> Self {
        settings_path()
            .ok()
            .and_then(|path| load_from(&path).ok())
            .unwrap_or_default()
    }

    /// Persists settings in the application's owner-only state directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory is unavailable, serialization
    /// fails, or the private atomic write cannot be completed.
    pub fn save(&self) -> Result<()> {
        let path = settings_path()?;
        let bytes = serde_json::to_vec_pretty(self).context("serialize voice settings")?;
        hh_protocol::atomic_write_private(&path, &bytes)
            .with_context(|| format!("write voice settings {}", path.display()))
    }
}

fn settings_path() -> Result<PathBuf> {
    hh_protocol::state_directory()
        .context("HOME is not set and HH_STATE_DIR is not configured")
        .map(|directory| directory.join(SETTINGS_FILE))
}

fn load_from(path: &Path) -> Result<VoiceSettings> {
    let bytes = hh_protocol::read_private_file(path, MAX_SETTINGS_BYTES)
        .with_context(|| format!("read voice settings {}", path.display()))?;
    serde_json::from_slice(&bytes).context("decode voice settings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_stable_and_unknown_fields_are_rejected() {
        let defaults = VoiceSettings::default();
        assert_eq!(defaults.schema_version, 1);
        assert_eq!(defaults.model, "gpt-realtime-2.1");
        assert_eq!(defaults.voice, "marin");
        assert_eq!(defaults.idle_timeout_secs, 900);
        assert!(!defaults.full_duplex);
        assert!(defaults.honcho.is_none());

        let error = serde_json::from_value::<VoiceSettings>(serde_json::json!({
            "unexpected": true
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn settings_round_trip_without_exposing_implicit_fields() {
        let settings = VoiceSettings {
            api_key: "secret".to_owned(),
            model: "gpt-realtime-2.1-mini".to_owned(),
            voice: "cedar".to_owned(),
            full_duplex: true,
            idle_timeout_secs: 0,
            honcho: Some(HonchoSettings::default()),
            ..VoiceSettings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            serde_json::from_str::<VoiceSettings>(&json).unwrap(),
            settings
        );
    }
}
