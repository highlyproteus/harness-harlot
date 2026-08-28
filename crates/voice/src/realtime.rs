use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
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
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(1);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECT_RETRIES: u32 = 8;
const STABLE_CONNECTION_RESET_AFTER: Duration = Duration::from_secs(30);
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
    pub(crate) fn new(instructions: String, voice: String) -> Self {
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

impl RealtimeInbound {
    const fn droppable_delta(&self) -> bool {
        matches!(
            self,
            Self::Event(
                ServerEvent::InputTranscriptionDelta { .. }
                    | ServerEvent::OutputAudioDelta { .. }
                    | ServerEvent::OutputTranscriptDelta { .. }
            )
        )
    }
}

fn forward_inbound(
    inbound: &SyncSender<RealtimeInbound>,
    mut event: RealtimeInbound,
    shutdown_requested: &AtomicBool,
) -> bool {
    loop {
        match inbound.try_send(event) {
            Ok(()) => return true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                if returned.droppable_delta() {
                    return false;
                }
                if shutdown_requested.load(Ordering::Acquire) {
                    return false;
                }
                event = returned;
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

pub(crate) type RecoverableSendResult =
    std::result::Result<(), (anyhow::Error, Option<ClientEvent>)>;

#[derive(Debug)]
enum WsCommand {
    Event {
        event: Box<ClientEvent>,
        ack: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
        delivery: Arc<DeliveryState>,
    },
    Shutdown,
}

const DELIVERY_QUEUED: u8 = 0;
const DELIVERY_SENDING: u8 = 1;
const DELIVERY_DELIVERED: u8 = 2;
const DELIVERY_CANCELLED: u8 = 3;
const DELIVERY_UNSENT: u8 = 4;

#[derive(Debug)]
struct DeliveryState(AtomicU8);

impl DeliveryState {
    fn queued() -> Self {
        Self(AtomicU8::new(DELIVERY_QUEUED))
    }

    fn begin_delivery(&self) -> bool {
        match self.0.compare_exchange(
            DELIVERY_QUEUED,
            DELIVERY_SENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(DELIVERY_SENDING) => true,
            Err(_) => false,
        }
    }

    fn mark_delivered(&self) {
        self.0.store(DELIVERY_DELIVERED, Ordering::Release);
    }

    fn cancel_before_delivery(&self) -> bool {
        self.0
            .compare_exchange(
                DELIVERY_QUEUED,
                DELIVERY_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn mark_unsent_if_queued(&self) {
        let _ = self.0.compare_exchange(
            DELIVERY_QUEUED,
            DELIVERY_UNSENT,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn definitely_unsent(&self) -> bool {
        matches!(
            self.0.load(Ordering::Acquire),
            DELIVERY_CANCELLED | DELIVERY_UNSENT
        )
    }
}

#[derive(Debug)]
pub(crate) struct RealtimeHandle {
    outbound: tokio::sync::mpsc::Sender<WsCommand>,
    shutdown_requested: Arc<AtomicBool>,
    shutdown_wake: Arc<tokio::sync::Notify>,
    join: Option<JoinHandle<()>>,
}

impl RealtimeHandle {
    pub(crate) fn send(&self, event: ClientEvent) -> Result<()> {
        let (ack, _result) = std::sync::mpsc::sync_channel(1);
        let command = WsCommand::Event {
            event: Box::new(event),
            ack,
            delivery: Arc::new(DeliveryState::queued()),
        };
        self.outbound
            .try_send(command)
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    anyhow::anyhow!("Realtime outbound queue is full")
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    anyhow::anyhow!("Realtime websocket task stopped")
                }
            })
    }

    #[cfg(test)]
    pub(crate) fn send_recoverable(&self, event: ClientEvent) -> RecoverableSendResult {
        send_recoverable_with_outbound(&self.outbound, event)
    }

    pub(crate) fn send_recoverable_async(
        &self,
        event: ClientEvent,
    ) -> Result<Receiver<RecoverableSendResult>> {
        let outbound = self.outbound.clone();
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("hh-realtime-send".to_owned())
            .spawn(move || {
                let _ = result_tx.send(send_recoverable_with_outbound(&outbound, event));
            })
            .context("spawn Realtime acknowledgement waiter")?;
        Ok(result_rx)
    }

    pub(crate) fn shutdown(mut self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.shutdown_wake.notify_one();
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

fn send_recoverable_with_outbound(
    outbound: &tokio::sync::mpsc::Sender<WsCommand>,
    event: ClientEvent,
) -> RecoverableSendResult {
    let (ack, result) = std::sync::mpsc::sync_channel(1);
    let delivery = Arc::new(DeliveryState::queued());
    let command = WsCommand::Event {
        event: Box::new(event.clone()),
        ack,
        delivery: Arc::clone(&delivery),
    };
    if let Err(error) = outbound.try_send(command) {
        let message = match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => "Realtime outbound queue is full",
            tokio::sync::mpsc::error::TrySendError::Closed(_) => "Realtime websocket task stopped",
        };
        return Err((anyhow::anyhow!(message), Some(event)));
    }
    match result.recv_timeout(SEND_ACK_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err((
            anyhow::anyhow!(error),
            delivery.definitely_unsent().then_some(event),
        )),
        Err(error) => {
            let unsent = delivery.cancel_before_delivery().then_some(event);
            Err((
                anyhow::anyhow!("Realtime send acknowledgement failed: {error}"),
                unsent,
            ))
        }
    }
}

pub(crate) fn spawn(
    api_key: String,
    model: String,
    inbound: SyncSender<RealtimeInbound>,
) -> Result<RealtimeHandle> {
    if api_key.trim().is_empty() {
        anyhow::bail!("OpenAI API key is empty");
    }
    let (outbound, receiver) = tokio::sync::mpsc::channel(OUTBOUND_CAPACITY);
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let shutdown_wake = Arc::new(tokio::sync::Notify::new());
    let worker_shutdown_requested = Arc::clone(&shutdown_requested);
    let worker_shutdown_wake = Arc::clone(&shutdown_wake);
    let join = std::thread::Builder::new()
        .name("hh-realtime".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build();
            match runtime {
                Ok(runtime) => runtime.block_on(run_socket(
                    api_key,
                    model,
                    receiver,
                    inbound,
                    worker_shutdown_requested,
                    worker_shutdown_wake,
                )),
                Err(error) => {
                    let _ = forward_inbound(
                        &inbound,
                        RealtimeInbound::Disconnected(format!("build Realtime runtime: {error}")),
                        &worker_shutdown_requested,
                    );
                }
            }
        })
        .context("spawn Realtime websocket thread")?;
    Ok(RealtimeHandle {
        outbound,
        shutdown_requested,
        shutdown_wake,
        join: Some(join),
    })
}

async fn send_event<S>(sink: &mut S, event: &ClientEvent) -> std::result::Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let json = encode_event(event)?;
    send_message(sink, Message::Text(Utf8Bytes::from(json))).await
}

async fn send_message<S>(sink: &mut S, message: Message) -> std::result::Result<(), String>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    tokio::time::timeout(SOCKET_IO_TIMEOUT, sink.send(message))
        .await
        .map_err(|_| format!("Realtime socket send timed out after {SOCKET_IO_TIMEOUT:?}"))?
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

#[derive(Default)]
struct ReconnectBudget {
    failures: u32,
}

impl ReconnectBudget {
    fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    fn record_disconnect(&mut self, connected_for: Duration) {
        if connected_for >= STABLE_CONNECTION_RESET_AFTER {
            self.failures = 0;
        }
        self.record_failure();
    }

    fn retryable(&self, status: Option<u16>) -> bool {
        provider_failure_retryable(status, self.failures)
    }
}

fn retain_after_send_failure(delivery: &DeliveryState) -> bool {
    delivery.definitely_unsent()
}

async fn run_socket(
    api_key: String,
    model: String,
    mut outbound: tokio::sync::mpsc::Receiver<WsCommand>,
    inbound: SyncSender<RealtimeInbound>,
    shutdown_requested: Arc<AtomicBool>,
    shutdown_wake: Arc<tokio::sync::Notify>,
) {
    let mut backoff_secs = 1_u64;
    let mut reconnect_budget = ReconnectBudget::default();
    let mut pending = None;
    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            return;
        }
        let mut request =
            match format!("wss://api.openai.com/v1/realtime?model={model}").into_client_request() {
                Ok(request) => request,
                Err(error) => {
                    let _ = forward_inbound(
                        &inbound,
                        RealtimeInbound::Disconnected(format!("build Realtime request: {error}")),
                        &shutdown_requested,
                    );
                    return;
                }
            };
        let authorization = match format!("Bearer {api_key}").parse() {
            Ok(value) => value,
            Err(error) => {
                let _ = forward_inbound(
                    &inbound,
                    RealtimeInbound::Disconnected(format!("invalid API key header: {error}")),
                    &shutdown_requested,
                );
                return;
            }
        };
        request.headers_mut().insert(AUTHORIZATION, authorization);

        let socket_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_PROVIDER_EVENT_BYTES))
            .max_frame_size(Some(MAX_PROVIDER_EVENT_BYTES))
            .max_write_buffer_size(MAX_PROVIDER_EVENT_BYTES * 2);
        let connect = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(request, Some(socket_config), false),
        );
        tokio::pin!(connect);
        let connect_result = tokio::select! {
            result = &mut connect => result,
            () = shutdown_wake.notified() => {
                if shutdown_requested.load(Ordering::Acquire) {
                    return;
                }
                continue;
            }
        };
        match connect_result {
            Ok(Ok((socket, _))) => {
                let connected_at = std::time::Instant::now();
                let _ = forward_inbound(&inbound, RealtimeInbound::Connected, &shutdown_requested);
                let (mut sink, mut stream) = socket.split();
                loop {
                    if shutdown_requested.load(Ordering::Acquire) {
                        let _ = send_message(&mut sink, Message::Close(None)).await;
                        return;
                    }
                    if let Some(command) = pending.take() {
                        let WsCommand::Event {
                            event,
                            ack,
                            delivery,
                        } = command
                        else {
                            return;
                        };
                        if !delivery.begin_delivery() {
                            let _ = ack.send(Err(
                                "Realtime send was cancelled before delivery".to_owned()
                            ));
                            continue;
                        }
                        if let Err(error) = send_event(&mut sink, &event).await {
                            if retain_after_send_failure(&delivery) {
                                pending = Some(WsCommand::Event {
                                    event,
                                    ack,
                                    delivery,
                                });
                            } else {
                                let _ = ack.send(Err(format!(
                                    "{error}; delivery is indeterminate and will not be retried"
                                )));
                            }
                            let _ = forward_inbound(
                                &inbound,
                                RealtimeInbound::Disconnected(error),
                                &shutdown_requested,
                            );
                            break;
                        }
                        delivery.mark_delivered();
                        let _ = ack.send(Ok(()));
                        continue;
                    }
                    tokio::select! {
                        command = outbound.recv() => match command {
                            Some(command @ WsCommand::Event { .. }) => pending = Some(command),
                            Some(WsCommand::Shutdown) | None => {
                                let _ = send_message(&mut sink, Message::Close(None)).await;
                                return;
                            }
                        },
                        () = shutdown_wake.notified() => {
                            if shutdown_requested.load(Ordering::Acquire) {
                                let _ = send_message(&mut sink, Message::Close(None)).await;
                                return;
                            }
                        }
                        message = stream.next() => match message {
                            Some(Ok(Message::Text(text))) => match decode_server_event(&text) {
                                Ok(event) => { let _ = forward_inbound(&inbound, RealtimeInbound::Event(event), &shutdown_requested); }
                                Err(error) => { let _ = forward_inbound(&inbound, RealtimeInbound::Warning(error), &shutdown_requested); }
                            },
                            Some(Ok(Message::Close(frame))) => {
                                let _ = forward_inbound(&inbound, RealtimeInbound::Disconnected(format!("Realtime socket closed: {frame:?}")), &shutdown_requested);
                                break;
                            }
                            Some(Ok(Message::Ping(payload))) => {
                                if send_message(&mut sink, Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(error)) => {
                                let _ = forward_inbound(&inbound, RealtimeInbound::Disconnected(format!("read Realtime socket: {error}")), &shutdown_requested);
                                break;
                            }
                            None => {
                                let _ = forward_inbound(&inbound, RealtimeInbound::Disconnected("Realtime socket ended".to_owned()), &shutdown_requested);
                                break;
                            }
                        }
                    }
                }
                let connected_for = connected_at.elapsed();
                reconnect_budget.record_disconnect(connected_for);
                if connected_for >= STABLE_CONNECTION_RESET_AFTER {
                    backoff_secs = 1;
                }
                if !reconnect_budget.retryable(None) {
                    if let Some(WsCommand::Event { ack, .. }) = pending.take() {
                        let _ = ack.send(Err("Realtime reconnect limit reached".to_owned()));
                    }
                    return;
                }
            }
            Ok(Err(error)) => {
                reconnect_budget.record_failure();
                let status = match &error {
                    tokio_tungstenite::tungstenite::Error::Http(response) => {
                        Some(response.status().as_u16())
                    }
                    _ => None,
                };
                let message = format!("connect Realtime socket: {error}");
                let _ = forward_inbound(
                    &inbound,
                    RealtimeInbound::Disconnected(message.clone()),
                    &shutdown_requested,
                );
                if !reconnect_budget.retryable(status) {
                    if let Some(WsCommand::Event { ack, delivery, .. }) = pending.take() {
                        delivery.mark_unsent_if_queued();
                        let _ = ack.send(Err(message));
                    }
                    return;
                }
            }
            Err(_) => {
                reconnect_budget.record_failure();
                let message =
                    format!("connect Realtime socket timed out after {CONNECT_TIMEOUT:?}");
                let _ = forward_inbound(
                    &inbound,
                    RealtimeInbound::Disconnected(message.clone()),
                    &shutdown_requested,
                );
                if !reconnect_budget.retryable(None) {
                    if let Some(WsCommand::Event { ack, delivery, .. }) = pending.take() {
                        delivery.mark_unsent_if_queued();
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
            () = shutdown_wake.notified() => {
                if shutdown_requested.load(Ordering::Acquire) {
                    return;
                }
            }
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
    use futures::Sink;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context as TaskContext, Poll};

    fn test_handle(outbound: tokio::sync::mpsc::Sender<WsCommand>) -> RealtimeHandle {
        RealtimeHandle {
            outbound,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            shutdown_wake: Arc::new(tokio::sync::Notify::new()),
            join: None,
        }
    }
    #[test]
    #[ignore = "writes the live session.update payload for manual debugging"]
    fn dump_session_update_payload() {
        let event = ClientEvent::SessionUpdate {
            session: Box::new(SessionConfig::new("debug".to_owned(), "alloy".to_owned())),
        };
        std::fs::write(
            "/tmp/hh-session-update.json",
            serde_json::to_string_pretty(&event).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn realtime_session_advertises_no_provider_tools() {
        let session = SessionConfig::new("conversation only".to_owned(), "alloy".to_owned());
        let payload = serde_json::to_value(session).unwrap();
        assert!(payload.get("tools").is_none());
        assert!(
            payload
                .get("tool_choice")
                .is_none_or(|value| value == "none"),
            "tool choice must be absent or none"
        );
    }

    #[test]
    fn bounded_outbound_rejects_when_full_and_returns_unsent_event() {
        let (outbound, _receiver) = tokio::sync::mpsc::channel(1);
        let (ack_tx, _ack_rx) = std::sync::mpsc::sync_channel(1);
        outbound
            .try_send(WsCommand::Event {
                event: Box::new(ClientEvent::ResponseCreate { response: None }),
                ack: ack_tx,
                delivery: Arc::new(DeliveryState::queued()),
            })
            .unwrap();
        let handle = test_handle(outbound);
        let event = ClientEvent::ResponseCancel;
        let (error, recovered) = handle
            .send_recoverable(event.clone())
            .expect_err("a full realtime queue must apply backpressure");
        assert!(error.to_string().contains("queue is full"));
        assert_eq!(recovered, Some(event));
    }

    #[test]
    fn critical_provider_events_wait_for_bounded_capacity_instead_of_dropping() {
        let (inbound, receiver) = std::sync::mpsc::sync_channel(1);
        inbound.send(RealtimeInbound::Connected).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = std::thread::spawn(move || {
            forward_inbound(
                &inbound,
                RealtimeInbound::Disconnected("critical".to_owned()),
                &worker_shutdown,
            )
        });

        assert!(matches!(
            receiver.recv().unwrap(),
            RealtimeInbound::Connected
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)).unwrap(),
            RealtimeInbound::Disconnected(message) if message == "critical"
        ));
        assert!(worker.join().unwrap());
    }

    #[test]
    fn websocket_send_failure_after_delivery_begins_is_not_retained_for_retry() {
        let delivery = DeliveryState::queued();
        assert!(delivery.begin_delivery());

        assert!(!retain_after_send_failure(&delivery));
    }

    #[test]
    fn ack_timeout_never_returns_an_event_that_is_delivered_late() {
        let (outbound, mut receiver) = tokio::sync::mpsc::channel(1);
        let delivered = Arc::new(AtomicBool::new(false));
        let worker_delivered = Arc::clone(&delivered);
        let worker = std::thread::spawn(move || {
            let WsCommand::Event { ack, delivery, .. } = receiver.blocking_recv().unwrap() else {
                panic!("expected event command");
            };
            assert!(delivery.begin_delivery());
            std::thread::sleep(SEND_ACK_TIMEOUT + Duration::from_millis(50));
            worker_delivered.store(true, Ordering::Release);
            delivery.mark_delivered();
            let _ = ack.send(Ok(()));
        });
        let handle = test_handle(outbound);

        let recovered = handle
            .send_recoverable(ClientEvent::ResponseCancel)
            .expect_err("the delayed acknowledgement must exceed the caller bound")
            .1;
        worker.join().unwrap();

        assert!(
            recovered.is_none() || !delivered.load(Ordering::Acquire),
            "event {recovered:?} was returned as unsent and also delivered"
        );
    }

    #[test]
    fn control_send_does_not_wait_for_a_provider_acknowledgement() {
        let (outbound, mut receiver) = tokio::sync::mpsc::channel(1);
        let worker = std::thread::spawn(move || {
            let _command = receiver.blocking_recv().unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        let handle = test_handle(outbound);

        let started = std::time::Instant::now();
        handle.send(ClientEvent::ResponseCancel).unwrap();

        assert!(
            started.elapsed() < Duration::from_millis(100),
            "engine control send blocked for {:?}",
            started.elapsed()
        );
        worker.join().unwrap();
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
    fn successful_handshakes_do_not_reset_a_flapping_reconnect_budget() {
        let mut budget = ReconnectBudget::default();
        for _ in 0..MAX_CONNECT_RETRIES {
            budget.record_disconnect(Duration::from_millis(10));
        }

        assert!(!budget.retryable(None));
    }

    #[test]
    fn shutdown_is_out_of_band_when_the_outbound_queue_is_full() {
        let (outbound, _receiver) = tokio::sync::mpsc::channel(1);
        let (ack, _result) = std::sync::mpsc::sync_channel(1);
        outbound
            .try_send(WsCommand::Event {
                event: Box::new(ClientEvent::ResponseCancel),
                ack,
                delivery: Arc::new(DeliveryState::queued()),
            })
            .unwrap();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown_requested);
        let join = std::thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let handle = RealtimeHandle {
            outbound,
            shutdown_requested,
            shutdown_wake: Arc::new(tokio::sync::Notify::new()),
            join: Some(join),
        };

        let started = std::time::Instant::now();
        handle.shutdown();

        assert!(started.elapsed() < Duration::from_millis(250));
    }

    struct StalledSink;

    impl Sink<Message> for StalledSink {
        type Error = std::io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(
            self: Pin<&mut Self>,
            _item: Message,
        ) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Pending
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_socket_send_returns_within_the_io_bound() {
        let started = std::time::Instant::now();
        let error = send_event(&mut StalledSink, &ClientEvent::ResponseCancel)
            .await
            .unwrap_err();

        assert!(error.contains("timed out"), "unexpected error: {error}");
        assert!(started.elapsed() < SOCKET_IO_TIMEOUT + Duration::from_millis(250));
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
                    }
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
