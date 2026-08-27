use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

const OUTBOUND_CAPACITY: usize = 256;
const SEND_ACK_TIMEOUT: Duration = Duration::from_secs(6);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECT_RETRIES: u32 = 8;
const MAX_PROVIDER_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum ClientEvent {
    #[serde(rename = "session.update")]
    SessionUpdate { session: Box<SessionConfig> },
    #[serde(rename = "input_audio_buffer.append")]
    InputAudioBufferAppend { audio: String },
    #[serde(rename = "conversation.item.create")]
    ConversationItemCreate {
        item: ConversationItem,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_item_id: Option<String>,
    },
    #[serde(rename = "conversation.item.truncate")]
    ConversationItemTruncate {
        item_id: String,
        content_index: u32,
        audio_end_ms: u64,
    },
    #[serde(rename = "response.create")]
    ResponseCreate {
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<Value>,
    },
    #[serde(rename = "response.cancel")]
    ResponseCancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ConversationItem {
    Message {
        role: ConversationRole,
        content: Vec<InputContent>,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConversationRole {
    System,
    User,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum InputContent {
    InputText { text: String },
    InputImage { image_url: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionConfig {
    #[serde(rename = "type")]
    session_type: String,
    output_modalities: [String; 1],
    instructions: String,
    reasoning: ReasoningConfig,
    truncation: TruncationConfig,
    audio: AudioConfig,
    tools: Vec<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ReasoningConfig {
    effort: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct TruncationConfig {
    #[serde(rename = "type")]
    truncation_type: String,
    retention_ratio: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AudioConfig {
    input: InputAudioConfig,
    output: OutputAudioConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct InputAudioConfig {
    format: PcmFormat,
    transcription: TranscriptionConfig,
    turn_detection: TurnDetectionConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct OutputAudioConfig {
    format: PcmFormat,
    voice: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PcmFormat {
    #[serde(rename = "type")]
    format_type: String,
    rate: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct TranscriptionConfig {
    model: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct TurnDetectionConfig {
    #[serde(rename = "type")]
    detection_type: String,
    eagerness: String,
    create_response: bool,
    interrupt_response: bool,
}

impl SessionConfig {
    pub(crate) fn new(instructions: String, voice: String, tools: Vec<Value>) -> Self {
        Self {
            session_type: "realtime".to_owned(),
            output_modalities: ["audio".to_owned()],
            instructions,
            reasoning: ReasoningConfig {
                effort: "low".to_owned(),
            },
            truncation: TruncationConfig {
                truncation_type: "retention_ratio".to_owned(),
                retention_ratio: 0.8,
            },
            audio: AudioConfig {
                input: InputAudioConfig {
                    format: PcmFormat {
                        format_type: "audio/pcm".to_owned(),
                        rate: 24_000,
                    },
                    transcription: TranscriptionConfig {
                        model: "gpt-4o-mini-transcribe".to_owned(),
                    },
                    turn_detection: TurnDetectionConfig {
                        detection_type: "semantic_vad".to_owned(),
                        eagerness: "auto".to_owned(),
                        create_response: true,
                        interrupt_response: true,
                    },
                },
                output: OutputAudioConfig {
                    format: PcmFormat {
                        format_type: "audio/pcm".to_owned(),
                        rate: 24_000,
                    },
                    voice,
                },
            },
            tools,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum ServerEvent {
    #[serde(rename = "session.created")]
    SessionCreated { session: Value },
    #[serde(rename = "session.updated")]
    SessionUpdated { session: Value },
    #[serde(rename = "error")]
    Error { error: ServerError },
    #[serde(rename = "input_audio_buffer.speech_started")]
    SpeechStarted {
        audio_start_ms: u64,
        item_id: String,
    },
    #[serde(rename = "input_audio_buffer.speech_stopped")]
    SpeechStopped {
        #[serde(default)]
        audio_end_ms: Option<u64>,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "input_audio_buffer.committed")]
    AudioCommitted {
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        previous_item_id: Option<String>,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.delta")]
    InputTranscriptionDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    InputTranscriptionCompleted {
        transcript: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.created")]
    ResponseCreated { response: RealtimeResponse },
    #[serde(rename = "response.done")]
    ResponseDone { response: RealtimeResponse },
    #[serde(rename = "response.output_audio.delta")]
    OutputAudioDelta { item_id: String, delta: String },
    #[serde(rename = "response.output_audio_transcript.delta")]
    OutputTranscriptDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.output_audio_transcript.done")]
    OutputTranscriptDone {
        transcript: String,
        #[serde(default)]
        item_id: Option<String>,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "rate_limits.updated")]
    RateLimitsUpdated {
        #[serde(default)]
        rate_limits: Vec<Value>,
    },
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ServerError {
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(default)]
    pub code: Option<String>,
    pub message: String,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct RealtimeResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub output: Vec<Value>,
    #[serde(default)]
    pub usage: Option<ResponseUsage>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResponseUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub input_token_details: InputTokenDetails,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct InputTokenDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

#[derive(Debug)]
pub(crate) enum RealtimeInbound {
    Connected,
    Event(ServerEvent),
    Warning(String),
    Disconnected(String),
}

#[derive(Debug)]
enum WsCommand {
    Event {
        event: Box<ClientEvent>,
        ack: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
        cancelled: Arc<AtomicBool>,
    },
    Shutdown,
}

#[derive(Debug)]
pub(crate) struct RealtimeHandle {
    outbound: tokio::sync::mpsc::Sender<WsCommand>,
    join: Option<JoinHandle<()>>,
}

impl RealtimeHandle {
    pub(crate) fn send(&self, event: ClientEvent) -> Result<()> {
        self.send_recoverable(event).map_err(|(error, _)| error)
    }

    pub(crate) fn send_recoverable(
        &self,
        event: ClientEvent,
    ) -> std::result::Result<(), (anyhow::Error, ClientEvent)> {
        let (ack, result) = std::sync::mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let command = WsCommand::Event {
            event: Box::new(event.clone()),
            ack,
            cancelled: Arc::clone(&cancelled),
        };
        if let Err(error) = self.outbound.try_send(command) {
            let message = match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    "Realtime outbound queue is full"
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    "Realtime websocket task stopped"
                }
            };
            return Err((anyhow::anyhow!(message), event));
        }
        match result.recv_timeout(SEND_ACK_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err((anyhow::anyhow!(error), event)),
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                Err((
                    anyhow::anyhow!("Realtime send acknowledgement failed: {error}"),
                    event,
                ))
            }
        }
    }

    pub(crate) fn shutdown(mut self) {
        let _ = self.outbound.try_send(WsCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let deadline = std::time::Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
            while !join.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}

pub(crate) fn spawn(
    api_key: String,
    model: String,
    inbound: Sender<RealtimeInbound>,
) -> Result<RealtimeHandle> {
    if api_key.trim().is_empty() {
        anyhow::bail!("OpenAI API key is empty");
    }
    let (outbound, receiver) = tokio::sync::mpsc::channel(OUTBOUND_CAPACITY);
    let join = std::thread::Builder::new()
        .name("hh-realtime".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(run_socket(api_key, model, receiver, inbound)),
                Err(error) => {
                    let _ = inbound.send(RealtimeInbound::Disconnected(format!(
                        "build Realtime runtime: {error}"
                    )));
                }
            }
        })
        .context("spawn Realtime websocket thread")?;
    Ok(RealtimeHandle {
        outbound,
        join: Some(join),
    })
}

async fn send_event<S>(sink: &mut S, event: &ClientEvent) -> std::result::Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let json = encode_event(event)?;
    sink.send(Message::Text(Utf8Bytes::from(json)))
        .await
        .map_err(|error| format!("send Realtime event: {error}"))
}

fn encode_event(event: &ClientEvent) -> std::result::Result<String, String> {
    let json =
        serde_json::to_string(event).map_err(|error| format!("encode Realtime event: {error}"))?;
    if json.len() > MAX_PROVIDER_EVENT_BYTES {
        return Err(format!(
            "Realtime event exceeds {MAX_PROVIDER_EVENT_BYTES} byte limit"
        ));
    }
    Ok(json)
}

fn decode_server_event(text: &str) -> std::result::Result<ServerEvent, String> {
    if text.len() > MAX_PROVIDER_EVENT_BYTES {
        return Err(format!(
            "Realtime event exceeds {MAX_PROVIDER_EVENT_BYTES} byte limit"
        ));
    }
    serde_json::from_str(text).map_err(|error| format!("decode Realtime event: {error}"))
}

fn provider_failure_retryable(status: Option<u16>, retries: u32) -> bool {
    retries < MAX_CONNECT_RETRIES
        && !matches!(status, Some(400..=499) if status != Some(408) && status != Some(429))
}

async fn run_socket(
    api_key: String,
    model: String,
    mut outbound: tokio::sync::mpsc::Receiver<WsCommand>,
    inbound: Sender<RealtimeInbound>,
) {
    let mut backoff_secs = 1_u64;
    let mut retries = 0_u32;
    let mut pending = None;
    loop {
        let mut request =
            match format!("wss://api.openai.com/v1/realtime?model={model}").into_client_request() {
                Ok(request) => request,
                Err(error) => {
                    let _ = inbound.send(RealtimeInbound::Disconnected(format!(
                        "build Realtime request: {error}"
                    )));
                    return;
                }
            };
        let authorization = match format!("Bearer {api_key}").parse() {
            Ok(value) => value,
            Err(error) => {
                let _ = inbound.send(RealtimeInbound::Disconnected(format!(
                    "invalid API key header: {error}"
                )));
                return;
            }
        };
        request.headers_mut().insert(AUTHORIZATION, authorization);

        let socket_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_PROVIDER_EVENT_BYTES))
            .max_frame_size(Some(MAX_PROVIDER_EVENT_BYTES))
            .max_write_buffer_size(MAX_PROVIDER_EVENT_BYTES * 2);
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(request, Some(socket_config), false),
        )
        .await
        {
            Ok(Ok((socket, _))) => {
                backoff_secs = 1;
                retries = 0;
                let _ = inbound.send(RealtimeInbound::Connected);
                let (mut sink, mut stream) = socket.split();
                loop {
                    if let Some(command) = pending.take() {
                        let WsCommand::Event {
                            event,
                            ack,
                            cancelled,
                        } = command
                        else {
                            return;
                        };
                        if cancelled.load(Ordering::Acquire) {
                            let _ = ack.send(Err(
                                "Realtime send was cancelled before delivery".to_owned()
                            ));
                            continue;
                        }
                        if let Err(error) = send_event(&mut sink, &event).await {
                            pending = Some(WsCommand::Event {
                                event,
                                ack,
                                cancelled,
                            });
                            let _ = inbound.send(RealtimeInbound::Disconnected(error));
                            break;
                        }
                        let _ = ack.send(Ok(()));
                        continue;
                    }
                    tokio::select! {
                        command = outbound.recv() => match command {
                            Some(command @ WsCommand::Event { .. }) => pending = Some(command),
                            Some(WsCommand::Shutdown) | None => {
                                let _ = sink.send(Message::Close(None)).await;
                                return;
                            }
                        },
                        message = stream.next() => match message {
                            Some(Ok(Message::Text(text))) => match decode_server_event(&text) {
                                Ok(event) => { let _ = inbound.send(RealtimeInbound::Event(event)); }
                                Err(error) => { let _ = inbound.send(RealtimeInbound::Warning(error)); }
                            },
                            Some(Ok(Message::Close(frame))) => {
                                let _ = inbound.send(RealtimeInbound::Disconnected(format!("Realtime socket closed: {frame:?}")));
                                break;
                            }
                            Some(Ok(Message::Ping(payload))) => {
                                if sink.send(Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(error)) => {
                                let _ = inbound.send(RealtimeInbound::Disconnected(format!("read Realtime socket: {error}")));
                                break;
                            }
                            None => {
                                let _ = inbound.send(RealtimeInbound::Disconnected("Realtime socket ended".to_owned()));
                                break;
                            }
                        }
                    }
                }
            }
            Ok(Err(error)) => {
                retries = retries.saturating_add(1);
                let status = match &error {
                    tokio_tungstenite::tungstenite::Error::Http(response) => {
                        Some(response.status().as_u16())
                    }
                    _ => None,
                };
                let message = format!("connect Realtime socket: {error}");
                let _ = inbound.send(RealtimeInbound::Disconnected(message.clone()));
                if !provider_failure_retryable(status, retries) {
                    if let Some(WsCommand::Event { ack, .. }) = pending.take() {
                        let _ = ack.send(Err(message));
                    }
                    return;
                }
            }
            Err(_) => {
                retries = retries.saturating_add(1);
                let message =
                    format!("connect Realtime socket timed out after {CONNECT_TIMEOUT:?}");
                let _ = inbound.send(RealtimeInbound::Disconnected(message.clone()));
                if !provider_failure_retryable(None, retries) {
                    if let Some(WsCommand::Event { ack, .. }) = pending.take() {
                        let _ = ack.send(Err(message));
                    }
                    return;
                }
            }
        }

        let sleep = tokio::time::sleep(Duration::from_secs(backoff_secs));
        tokio::pin!(sleep);
        tokio::select! {
            () = &mut sleep => {}
            command = outbound.recv(), if pending.is_none() => match command {
                Some(WsCommand::Shutdown) | None => return,
                Some(command @ WsCommand::Event { .. }) => pending = Some(command),
            }
        }
        backoff_secs = (backoff_secs.saturating_mul(2)).min(30);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    #[ignore = "writes the live session.update payload for manual debugging"]
    fn dump_session_update_payload() {
        let event = ClientEvent::SessionUpdate {
            session: Box::new(SessionConfig::new(
                "debug".to_owned(),
                "alloy".to_owned(),
                crate::tools::tool_schemas(),
            )),
        };
        std::fs::write(
            "/tmp/hh-session-update.json",
            serde_json::to_string_pretty(&event).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn bounded_outbound_rejects_when_full_and_returns_unsent_event() {
        let (outbound, _receiver) = tokio::sync::mpsc::channel(1);
        let (ack_tx, _ack_rx) = std::sync::mpsc::sync_channel(1);
        outbound
            .try_send(WsCommand::Event {
                event: Box::new(ClientEvent::ResponseCreate { response: None }),
                ack: ack_tx,
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })
            .unwrap();
        let handle = RealtimeHandle {
            outbound,
            join: None,
        };
        let event = ClientEvent::ResponseCancel;
        let (error, recovered) = handle
            .send_recoverable(event.clone())
            .expect_err("a full realtime queue must apply backpressure");
        assert!(error.to_string().contains("queue is full"));
        assert_eq!(recovered, event);
    }

    #[test]
    fn permanent_provider_failures_are_not_retried_and_transient_failures_are_capped() {
        assert!(!provider_failure_retryable(Some(401), 0));
        assert!(!provider_failure_retryable(Some(403), 0));
        assert!(provider_failure_retryable(Some(429), 0));
        assert!(provider_failure_retryable(Some(500), 0));
        assert!(!provider_failure_retryable(Some(500), MAX_CONNECT_RETRIES));
    }

    #[test]
    fn provider_payloads_are_bounded_in_both_directions() {
        let oversized = ClientEvent::ConversationItemCreate {
            item: ConversationItem::Message {
                role: ConversationRole::User,
                content: vec![InputContent::InputText {
                    text: "x".repeat(MAX_PROVIDER_EVENT_BYTES),
                }],
            },
            previous_item_id: None,
        };
        assert!(encode_event(&oversized).unwrap_err().contains("exceeds"));

        let oversized_inbound = format!(
            "{{\"type\":\"response.output_audio_transcript.delta\",\"delta\":\"{}\"}}",
            "x".repeat(MAX_PROVIDER_EVENT_BYTES)
        );
        assert!(
            decode_server_event(&oversized_inbound)
                .unwrap_err()
                .contains("exceeds")
        );
    }

    #[test]
    fn every_client_event_round_trips_literal_ga_json() {
        let fixtures = [
            json!({
                "type": "session.update",
                "session": {
                    "type": "realtime",
                    "output_modalities": ["audio"],
                    "instructions": "manage it",
                    "reasoning": {"effort": "low"},
                    "truncation": {"type": "retention_ratio", "retention_ratio": 0.8},
                    "audio": {
                        "input": {
                            "format": {"type": "audio/pcm", "rate": 24000},
                            "transcription": {"model": "gpt-4o-mini-transcribe"},
                            "turn_detection": {
                                "type": "semantic_vad",
                                "eagerness": "auto",
                                "create_response": true,
                                "interrupt_response": true
                            }
                        },
                        "output": {
                            "format": {"type": "audio/pcm", "rate": 24000},
                            "voice": "marin"
                        }
                    },
                    "tools": []
                }
            }),
            json!({"type":"input_audio_buffer.append","audio":"AAE="}),
            json!({
                "type":"conversation.item.create",
                "item":{
                    "type":"message",
                    "role":"user",
                    "content":[{"type":"input_text","text":"hello"}]
                }
            }),
            json!({
                "type":"conversation.item.create",
                "item":{
                    "type":"function_call_output",
                    "call_id":"call-1",
                    "output":"{\"ok\":true}"
                },
                "previous_item_id":"item-1"
            }),
            json!({
                "type":"conversation.item.truncate",
                "item_id":"item-2",
                "content_index":0,
                "audio_end_ms":420
            }),
            json!({"type":"response.create"}),
            json!({"type":"response.cancel"}),
        ];
        for fixture in fixtures {
            let event: ClientEvent = serde_json::from_value(fixture.clone()).unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), fixture);
        }
    }

    #[test]
    fn user_message_serializes_text_and_image_content() {
        let event = ClientEvent::ConversationItemCreate {
            item: ConversationItem::Message {
                role: ConversationRole::User,
                content: vec![
                    InputContent::InputText {
                        text: "describe this".to_owned(),
                    },
                    InputContent::InputImage {
                        image_url: "data:image/png;base64,AAA".to_owned(),
                    },
                ],
            },
            previous_item_id: None,
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "conversation.item.create",
                "item": {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "describe this"},
                        {
                            "type": "input_image",
                            "image_url": "data:image/png;base64,AAA"
                        }
                    ]
                }
            })
        );
    }

    #[test]
    fn every_server_event_round_trips_literal_ga_json() {
        let fixtures = [
            json!({"type":"session.created","session":{"id":"sess-1"}}),
            json!({"type":"session.updated","session":{"id":"sess-1"}}),
            json!({"type":"error","error":{"type":"invalid_request_error","code":"bad","message":"bad request"}}),
            json!({"type":"input_audio_buffer.speech_started","audio_start_ms":12,"item_id":"item-1"}),
            json!({"type":"input_audio_buffer.speech_stopped","audio_end_ms":42,"item_id":"item-1"}),
            json!({"type":"input_audio_buffer.committed","item_id":"item-1","previous_item_id":"item-0"}),
            json!({"type":"conversation.item.input_audio_transcription.delta","delta":"hel","item_id":"item-1"}),
            json!({"type":"conversation.item.input_audio_transcription.completed","transcript":"hello","item_id":"item-1"}),
            json!({"type":"response.created","response":{"id":"resp-1","status":"in_progress","output":[],"usage":null}}),
            json!({"type":"response.done","response":{"id":"resp-1","status":"completed","output":[],"usage":{"input_tokens":12,"output_tokens":4,"input_token_details":{"cached_tokens":3}}}}),
            json!({"type":"response.output_audio.delta","item_id":"item-2","delta":"AAE="}),
            json!({"type":"response.output_audio_transcript.delta","delta":"hi","item_id":"item-2"}),
            json!({"type":"response.output_audio_transcript.done","transcript":"hi there","item_id":"item-2"}),
            json!({"type":"response.function_call_arguments.done","call_id":"call-1","name":"check_status","arguments":"{}"}),
            json!({"type":"rate_limits.updated","rate_limits":[{"name":"tokens","remaining":10}]}),
        ];
        for fixture in fixtures {
            let event: ServerEvent = serde_json::from_value(fixture.clone()).unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), fixture);
        }
        assert_eq!(
            serde_json::from_value::<ServerEvent>(json!({
                "type":"future.event",
                "anything":true
            }))
            .unwrap(),
            ServerEvent::Unknown
        );
    }
}
