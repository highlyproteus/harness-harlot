use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde_json::{Value, json};
use uuid::Uuid;

use super::discovery::PiInstall;

const MAX_PI_FRAME_BYTES: usize = 4 * 1024 * 1024;
const PI_SHUTDOWN_BOUND: Duration = Duration::from_secs(2);

pub(crate) struct PiSpawnArgs {
    pub(crate) extension_path: PathBuf,
    pub(crate) session_dir: PathBuf,
    pub(crate) system_prompt: String,
    pub(crate) model: Option<String>,
    pub(crate) resume_session: Option<PathBuf>,
    pub(crate) cwd: PathBuf,
    pub(crate) pane_id: Uuid,
    pub(crate) workspace_id: Uuid,
}

#[derive(Debug)]
pub(crate) struct PiProcess {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<String, SyncSender<Value>>>,
    next_id: AtomicU64,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr: Mutex<Option<JoinHandle<()>>>,
    stderr_lines: Mutex<VecDeque<String>>,
}

impl PiProcess {
    pub(crate) fn spawn(
        install: &PiInstall,
        args: &PiSpawnArgs,
        sink: Arc<dyn Fn(Value) + Send + Sync>,
    ) -> Result<Arc<Self>> {
        let socket_path = hh_protocol::socket_path().context("resolve assistant socket path")?;
        let mut command = Command::new(&install.program);
        command.args([
            "--mode",
            "rpc",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-builtin-tools",
            "--no-approve",
            "-e",
        ]);
        command.arg(&args.extension_path);
        command.arg("--session-dir").arg(&args.session_dir);
        command.arg("--system-prompt").arg(&args.system_prompt);
        if let Some(model) = &args.model {
            command.arg("--model").arg(model);
        }
        if let Some(session) = &args.resume_session {
            command.arg("--session").arg(session);
        }
        command
            .env("PATH", &install.login_path)
            .env("HH_SOCKET", socket_path)
            .env(
                "HH_PROTOCOL_VERSION",
                hh_protocol::PROTOCOL_VERSION.to_string(),
            )
            .env("HH_ASSISTANT_PANE_ID", args.pane_id.to_string())
            .env("HH_ASSISTANT_WORKSPACE_ID", args.workspace_id.to_string())
            .env("NO_COLOR", "1")
            .current_dir(&args.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("start pi from {}", install.program.display()))?;
        let stdin = child.stdin.take().context("pi stdin was not piped")?;
        let stdout = child.stdout.take().context("pi stdout was not piped")?;
        let stderr = child.stderr.take().context("pi stderr was not piped")?;
        let process = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            reader: Mutex::new(None),
            stderr: Mutex::new(None),
            stderr_lines: Mutex::new(VecDeque::with_capacity(20)),
        });

        let weak = Arc::downgrade(&process);
        let reader_sink = sink;
        let reader = thread::Builder::new()
            .name(format!("hh-pi-reader-{}", args.pane_id))
            .spawn(move || read_stdout(&weak, stdout, &reader_sink))
            .context("start pi stdout reader")?;
        *process.reader.lock() = Some(reader);

        let weak = Arc::downgrade(&process);
        let stderr_reader = thread::Builder::new()
            .name(format!("hh-pi-stderr-{}", args.pane_id))
            .spawn(move || read_stderr(&weak, stderr))
            .context("start pi stderr reader")?;
        *process.stderr.lock() = Some(stderr_reader);

        Ok(process)
    }

    pub(crate) fn request(&self, mut command: Value, timeout: Duration) -> Result<Value> {
        let id = format!("hh-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let object = command
            .as_object_mut()
            .context("pi RPC command must be a JSON object")?;
        object.insert("id".to_owned(), Value::String(id.clone()));
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending.lock().insert(id.clone(), sender);
        if let Err(error) = self.write_json(&command) {
            self.pending.lock().remove(&id);
            return Err(error);
        }
        let response = match receiver.recv_timeout(timeout) {
            Ok(response) => response,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().remove(&id);
                bail!(
                    "pi did not answer {id} within {} seconds",
                    timeout.as_secs()
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.pending.lock().remove(&id);
                bail!("pi exited before answering {id}");
            }
        };
        if response.get("success").and_then(Value::as_bool) == Some(true) {
            Ok(response.get("data").cloned().unwrap_or(Value::Null))
        } else {
            let error = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("pi RPC request failed");
            bail!("{error}")
        }
    }

    pub(crate) fn notify(&self, value: &Value) -> Result<()> {
        self.write_json(value)
    }

    fn write_json(&self, value: &Value) -> Result<()> {
        let mut encoded = serde_json::to_vec(value).context("encode pi RPC command")?;
        encoded.push(b'\n');
        let mut guard = self.stdin.lock();
        let stdin = guard.as_mut().context("pi is not running")?;
        stdin.write_all(&encoded).context("write pi RPC command")?;
        stdin.flush().context("flush pi RPC command")
    }

    pub(crate) fn stderr_message(&self) -> String {
        let lines = self.stderr_lines.lock();
        if lines.is_empty() {
            "pi exited".to_owned()
        } else {
            lines.iter().cloned().collect::<Vec<_>>().join(" | ")
        }
    }

    pub(crate) fn shutdown(&self) {
        self.stdin.lock().take();
        let deadline = Instant::now() + PI_SHUTDOWN_BOUND;
        let mut child = self.child.lock();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    drop(child);
                    thread::sleep(Duration::from_millis(10));
                    child = self.child.lock();
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        drop(child);
        join_thread_bounded(&self.reader);
        join_thread_bounded(&self.stderr);
    }
}

impl Drop for PiProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn read_stdout(
    process: &Weak<PiProcess>,
    stdout: impl std::io::Read,
    sink: &Arc<dyn Fn(Value) + Send + Sync>,
) {
    let mut reader = BufReader::new(stdout);
    let mut frame = Vec::new();
    loop {
        frame.clear();
        match reader.read_until(b'\n', &mut frame) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                sink(json!({"type":"__hh_exit","message":format!("read pi output: {error}")}));
                return;
            }
        }
        if frame.len() > MAX_PI_FRAME_BYTES {
            sink(json!({"type":"__hh_exit","message":"pi sent an oversized frame"}));
            return;
        }
        if frame.last() == Some(&b'\n') {
            frame.pop();
        }
        if frame.last() == Some(&b'\r') {
            frame.pop();
        }
        let value: Value = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(error) => {
                sink(
                    json!({"type":"__hh_exit","message":format!("pi sent invalid JSON: {error}")}),
                );
                return;
            }
        };
        let response_id = value
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| *kind == "response")
            .and_then(|_| value.get("id"))
            .and_then(Value::as_str);
        if let Some(id) = response_id
            && let Some(process) = process.upgrade()
            && let Some(waiter) = process.pending.lock().remove(id)
        {
            let _ = waiter.send(value);
            continue;
        }
        sink(value);
    }
    if let Some(process) = process.upgrade() {
        let error = process.stderr_message();
        for (_, waiter) in process.pending.lock().drain() {
            let _ = waiter.send(json!({"success":false,"error":error}));
        }
    }
    sink(json!({"type":"__hh_exit"}));
}

fn read_stderr(process: &Weak<PiProcess>, stderr: impl std::io::Read) {
    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else {
            break;
        };
        let Some(process) = process.upgrade() else {
            break;
        };
        let mut lines = process.stderr_lines.lock();
        if lines.len() == 20 {
            lines.pop_front();
        }
        lines.push_back(line);
    }
}

fn join_thread_bounded(handle: &Mutex<Option<JoinHandle<()>>>) {
    if handle
        .lock()
        .as_ref()
        .is_some_and(|handle| handle.thread().id() == thread::current().id())
    {
        handle.lock().take();
        return;
    }
    let deadline = Instant::now() + PI_SHUTDOWN_BOUND;
    while handle
        .lock()
        .as_ref()
        .is_some_and(|handle| !handle.is_finished())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    if let Some(handle) = handle.lock().take()
        && handle.is_finished()
    {
        let _ = handle.join();
    }
}
