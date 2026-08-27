use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::settings::HonchoSettings;

const HONCHO_TIMEOUT: Duration = Duration::from_secs(10);
const FLUSH_INTERVAL: Duration = Duration::from_secs(15);
const MAX_MESSAGE_BATCH: usize = 20;
const MAX_PREAMBLE_CHARS: usize = 1_500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    const fn peer_id(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

pub(crate) trait MemoryBackend: Send {
    fn record_turn(&mut self, role: Role, text: &str);
    fn recall(&mut self, query: &str) -> Result<String>;
    fn session_preamble(&mut self) -> Option<String>;
    fn flush(&mut self) {}
}

#[derive(Debug, Default)]
pub(crate) struct NullBackend;

impl MemoryBackend for NullBackend {
    fn record_turn(&mut self, _role: Role, _text: &str) {}

    fn recall(&mut self, _query: &str) -> Result<String> {
        Ok(json!({ "error": "memory disabled" }).to_string())
    }

    fn session_preamble(&mut self) -> Option<String> {
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BufferedMessage {
    role: Role,
    content: String,
}

#[derive(Debug)]
pub(crate) struct HonchoBackend {
    agent: ureq::Agent,
    base_url: String,
    workspace_path: String,
    session_id: String,
    bearer: Option<String>,
    buffered: Vec<BufferedMessage>,
    last_flush: Instant,
}

impl HonchoBackend {
    pub(crate) fn new(settings: HonchoSettings) -> Result<Self> {
        let base_url = settings.base_url.trim_end_matches('/').to_owned();
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            bail!("Honcho base URL must start with http:// or https://");
        }
        let workspace = encode_path_segment(&settings.workspace);
        let unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let session_id = format!("voice-{unix_seconds}");
        let agent = ureq::builder()
            .timeout_connect(HONCHO_TIMEOUT)
            .timeout(HONCHO_TIMEOUT)
            .build();
        let mut backend = Self {
            agent,
            base_url,
            workspace_path: format!("/v3/workspaces/{workspace}"),
            session_id,
            bearer: settings.bearer,
            buffered: Vec::new(),
            last_flush: Instant::now(),
        };
        backend.initialize(&settings.workspace)?;
        Ok(backend)
    }

    fn initialize(&mut self, workspace_id: &str) -> Result<()> {
        self.post("/v3/workspaces", &json!({ "id": workspace_id }))?;
        self.post(
            &format!("{}/peers", self.workspace_path),
            &json!({ "id": "user" }),
        )?;
        self.post(
            &format!("{}/peers", self.workspace_path),
            &json!({ "id": "assistant" }),
        )?;
        self.post(
            &format!("{}/sessions", self.workspace_path),
            &json!({
                "id": self.session_id,
                "peers": { "user": {}, "assistant": {} }
            }),
        )?;
        Ok(())
    }

    fn flush_buffer(&mut self) -> Result<()> {
        while !self.buffered.is_empty() {
            let take = self.buffered.len().min(MAX_MESSAGE_BATCH);
            let batch = self
                .buffered
                .iter()
                .take(take)
                .map(|message| {
                    json!({
                        "peer_id": message.role.peer_id(),
                        "content": message.content,
                    })
                })
                .collect::<Vec<_>>();
            self.post(
                &format!(
                    "{}/sessions/{}/messages",
                    self.workspace_path,
                    encode_path_segment(&self.session_id)
                ),
                &json!({ "messages": batch }),
            )?;
            self.buffered.drain(..take);
        }
        self.last_flush = Instant::now();
        Ok(())
    }

    fn post(&self, path: &str, payload: &Value) -> Result<Value> {
        let url = format!("{}{path}", self.base_url);
        let mut request = self
            .agent
            .post(&url)
            .set("content-type", "application/json");
        if let Some(token) = self.bearer.as_deref() {
            request = request.set("authorization", &format!("Bearer {token}"));
        }
        let body = serde_json::to_string(payload).context("encode Honcho request")?;
        let response = request
            .send_string(&body)
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        if !(status == 200 || status == 201) {
            bail!("Honcho POST {url} returned {status}");
        }
        let text = response
            .into_string()
            .with_context(|| format!("read Honcho response from {url}"))?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("decode Honcho response from {url}"))
    }
}

impl MemoryBackend for HonchoBackend {
    fn record_turn(&mut self, role: Role, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.buffered.push(BufferedMessage {
            role,
            content: text.to_owned(),
        });
        if (self.buffered.len() >= MAX_MESSAGE_BATCH || self.last_flush.elapsed() >= FLUSH_INTERVAL)
            && let Err(error) = self.flush_buffer()
        {
            eprintln!("Honcho memory flush failed: {error:#}");
        }
    }

    fn recall(&mut self, query: &str) -> Result<String> {
        if let Err(error) = self.flush_buffer() {
            eprintln!("Honcho pre-recall flush failed: {error:#}");
        }
        let response = self.post(
            &format!("{}/peers/user/chat", self.workspace_path),
            &json!({ "query": query, "reasoning_level": "low" }),
        )?;
        response
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Honcho chat response has no content")
    }

    fn session_preamble(&mut self) -> Option<String> {
        match self.post(
            &format!("{}/peers/user/representation", self.workspace_path),
            &json!({}),
        ) {
            Ok(response) => response
                .get("representation")
                .and_then(Value::as_str)
                .map(|representation| representation.chars().take(MAX_PREAMBLE_CHARS).collect()),
            Err(error) => {
                eprintln!("Honcho representation failed: {error:#}");
                None
            }
        }
    }

    fn flush(&mut self) {
        if let Err(error) = self.flush_buffer() {
            eprintln!("Honcho memory flush failed: {error:#}");
        }
    }
}

impl Drop for HonchoBackend {
    fn drop(&mut self) {
        self.flush();
    }
}

pub(crate) fn backend(settings: Option<HonchoSettings>) -> Result<Box<dyn MemoryBackend>> {
    settings.map_or_else(
        || Ok(Box::<NullBackend>::default() as Box<dyn MemoryBackend>),
        |settings| Ok(Box::new(HonchoBackend::new(settings)?) as Box<dyn MemoryBackend>),
    )
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honcho_path_segments_are_percent_encoded() {
        assert_eq!(
            encode_path_segment("harness harlot/voice"),
            "harness%20harlot%2Fvoice"
        );
    }

    #[test]
    fn null_memory_reports_disabled_without_side_effects() {
        let mut memory = NullBackend;
        memory.record_turn(Role::User, "hello");
        assert!(memory.recall("hello").unwrap().contains("memory disabled"));
        assert_eq!(memory.session_preamble(), None);
    }

    #[test]
    #[ignore = "requires a live self-hosted Honcho v3 instance"]
    fn honcho_live_session_accepts_and_lists_messages() {
        let base_url = std::env::var("HH_TEST_HONCHO_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8000".to_owned());
        let mut memory = HonchoBackend::new(HonchoSettings {
            base_url,
            workspace: "harness-harlot".to_owned(),
            bearer: None,
        })
        .unwrap();
        let session_id = memory.session_id.clone();
        memory.record_turn(Role::User, "Voice Mode Honcho smoke preference");
        memory.record_turn(Role::Assistant, "Preference recorded");
        memory.flush();

        let sessions = memory
            .post(
                &format!("{}/sessions/list", memory.workspace_path),
                &json!({}),
            )
            .unwrap();
        assert!(sessions.to_string().contains(&session_id));
    }
}
