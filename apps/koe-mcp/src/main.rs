//! Local-only MCP stdio adapter.
//!
//! Stdout is owned exclusively by [`Server::run`]. Diagnostics, including
//! panic output, remain on stderr. The server accepts one JSON-RPC object per
//! line and applies fixed request/output/concurrency limits.

use std::{
    collections::{HashMap, HashSet},
    fs,
    future::Future,
    io::{self, BufRead, IsTerminal as _, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use clap::Parser;
use koe_app::{RecorderCoordinator, RecordingConsent};
use koe_audio::{AudioBackend, AudioStream, CpalBackend, OpenSource, frame_ring};
use koe_core::{NetworkPolicy, OperationId, SessionId, SessionState, SourceKind};
use koe_model::{
    DigestAllowlist, FoundryLocalAdapter, InstallOptions, KoeModelManager, ModelError,
    ModelManager, ModelProgress, ModelSelector,
};
use koe_recording::{RecordingConfig, SessionManifest, TimelineBlock, TrackKind};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_RETAINED_OPERATIONS: usize = 64;
const MAX_EXPORT_FILES: usize = 100_000;
const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024 * 1024;
const MAX_EXPORT_DEPTH: usize = 32;
const OPERATION_DEADLINE: Duration = Duration::from_hours(1);
const CANCELLATION_GRACE: Duration = Duration::from_secs(5);
const OPERATION_DEADLINE_ERROR: &str = "KOE-MCP-OPERATION-DEADLINE";
const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Parser)]
#[command(name = "koe-mcp", about = "Sandboxed koe MCP stdio server")]
struct Args {
    /// App-owned root. Session/model access cannot escape this directory.
    #[arg(long)]
    data_root: PathBuf,
    /// Optional fixed root under which exports may be created.
    #[arg(long)]
    export_root: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum OperationState {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
struct OperationSnapshot {
    operation_id: OperationId,
    session_id: Option<SessionId>,
    state: OperationState,
    progress: u8,
    error_code: Option<&'static str>,
}

enum RecordingControl {
    Stop,
    Cancel,
}

struct Operation {
    snapshot: Arc<Mutex<OperationSnapshot>>,
    recording_control: Option<Sender<RecordingControl>>,
    cancellation: tokio_util::sync::CancellationToken,
    task: Option<JoinHandle<()>>,
    progress_token: Option<Value>,
    last_notified_progress: u8,
}

#[derive(Debug, Error)]
enum McpError {
    #[error("invalid JSON")]
    ParseError,
    #[error("invalid request")]
    InvalidRequest,
    #[error("method not found")]
    MethodNotFound,
    #[error("invalid parameters")]
    InvalidParams,
    #[error("explicit host consent is required")]
    ConsentRequired,
    #[error("request exceeds the configured limit")]
    RequestTooLarge,
    #[error("response exceeds the configured limit")]
    ResponseTooLarge,
    #[error("path or session authorization failed")]
    Unauthorized,
    #[error("resource not found")]
    NotFound,
    #[error("operation capacity reached")]
    Capacity,
    #[error("operation failed")]
    OperationFailed,
}

impl McpError {
    const fn rpc_code(&self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest | Self::RequestTooLarge => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::NotFound => -32004,
            Self::ConsentRequired | Self::Unauthorized => -32003,
            Self::Capacity => -32008,
            Self::OperationFailed | Self::ResponseTooLarge => -32603,
        }
    }

    const fn koe_code(&self) -> &'static str {
        match self {
            Self::ConsentRequired => "KOE-POLICY-CONSENT-REQUIRED",
            Self::Unauthorized => "KOE-STORE-PATH-REJECTED",
            Self::NotFound => "KOE-SESSION-NOT-FOUND",
            Self::Capacity => "KOE-SESSION-CONFLICT",
            Self::RequestTooLarge => "KOE-MCP-REQUEST-TOO-LARGE",
            Self::ResponseTooLarge => "KOE-MCP-RESPONSE-TOO-LARGE",
            Self::InvalidParams | Self::InvalidRequest | Self::ParseError => {
                "KOE-MCP-INVALID-REQUEST"
            },
            Self::MethodNotFound => "KOE-MCP-METHOD-NOT-FOUND",
            Self::OperationFailed => "KOE-MCP-OPERATION-FAILED",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleState {
    Uninitialized,
    Initializing,
    Initialized,
}

struct Server {
    data_root: PathBuf,
    sessions_root: PathBuf,
    export_root: Option<PathBuf>,
    backend: CpalBackend,
    model_manager: Arc<KoeModelManager>,
    operations: HashMap<String, Operation>,
    authorized_resources: HashSet<String>,
    lifecycle: LifecycleState,
}

impl Server {
    fn new(args: &Args) -> Result<Self, McpError> {
        let data_root = authorize_root(&args.data_root, true)?;
        let sessions_root = authorize_root(&data_root.join("sessions"), true)?;
        let export_root = args
            .export_root
            .as_deref()
            .map(|path| authorize_root(path, false))
            .transpose()?;
        if export_root
            .as_ref()
            .is_some_and(|root| root.starts_with(&sessions_root) || sessions_root.starts_with(root))
        {
            return Err(McpError::Unauthorized);
        }
        let model_manager = Arc::new(
            KoeModelManager::new(
                &data_root,
                DigestAllowlist::empty(),
                Box::new(FoundryLocalAdapter::new()),
                NetworkPolicy::Denied,
            )
            .map_err(|_| McpError::OperationFailed)?,
        );
        Ok(Self {
            data_root,
            sessions_root,
            export_root,
            backend: CpalBackend::default(),
            model_manager,
            operations: HashMap::new(),
            authorized_resources: HashSet::new(),
            lifecycle: LifecycleState::Uninitialized,
        })
    }

    fn run(
        &mut self,
        mut input: impl BufRead + Send,
        mut output: impl Write,
    ) -> io::Result<()> {
        let result = thread::scope(|scope| {
            let (frames, receiver) = mpsc::channel();
            scope.spawn(move || {
                loop {
                    let frame = read_frame(&mut input);
                    let finished = matches!(frame, Ok(None) | Err(_));
                    if frames.send(frame).is_err() || finished {
                        break;
                    }
                }
            });
            loop {
                match receiver.recv_timeout(Duration::from_millis(25)) {
                    Ok(Ok(Some(frame))) => {
                        self.reap_operations();
                        let response = match frame {
                            Frame::Message(line) => self.handle_line(&line),
                            Frame::Oversized => {
                                Some(error_response(Value::Null, &McpError::RequestTooLarge))
                            },
                        };
                        if let Some(response) = response {
                            write_message(&mut output, &response)?;
                        }
                    },
                    Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Err(error)) => return Err(error),
                    Err(mpsc::RecvTimeoutError::Timeout) => self.reap_operations(),
                }
                self.emit_progress(&mut output)?;
            }
            Ok(())
        });
        self.cancel_all();
        result
    }

    fn handle_line(
        &mut self,
        line: &str,
    ) -> Option<Value> {
        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return Some(error_response(Value::Null, &McpError::ParseError)),
        };
        let id = request.get("id").cloned();
        if id.as_ref().is_some_and(|value| {
            !matches!(value, Value::Null | Value::String(_) | Value::Number(_))
        }) {
            return Some(error_response(Value::Null, &McpError::InvalidRequest));
        }
        let method = request.get("method").and_then(Value::as_str);
        if !request.is_object()
            || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || method.is_none()
        {
            return Some(error_response(
                id.unwrap_or(Value::Null),
                &McpError::InvalidRequest,
            ));
        }
        let method = method.unwrap_or_default();
        if id.is_none() {
            self.handle_notification(method, request.get("params"));
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        tracing::debug!(method, "handling request");
        match self.handle_request(method, request.get("params")) {
            Ok(result) => Some(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Err(error) => Some(error_response(id, &error)),
        }
    }

    fn handle_notification(
        &mut self,
        method: &str,
        _params: Option<&Value>,
    ) {
        if method == "notifications/initialized" && self.lifecycle == LifecycleState::Initializing {
            self.lifecycle = LifecycleState::Initialized;
        }
        // Standard `notifications/cancelled` names an in-flight JSON-RPC
        // request. All potentially long koe calls return an operation ID
        // immediately, so detached work is cancelled only through the
        // consented `koe_cancel_operation` tool.
    }

    fn handle_request(
        &mut self,
        method: &str,
        params: Option<&Value>,
    ) -> Result<Value, McpError> {
        if method == "initialize" {
            let valid_params = params.is_some_and(|value| {
                value
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .is_some()
                    && value.get("capabilities").is_some_and(Value::is_object)
                    && value.get("clientInfo").is_some_and(Value::is_object)
            });
            if self.lifecycle != LifecycleState::Uninitialized || !valid_params {
                return Err(McpError::InvalidRequest);
            }
            self.lifecycle = LifecycleState::Initializing;
            return Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}, "resources": {"subscribe": false, "listChanged": false}},
                "serverInfo": {"name": "koe", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Run as a least-privilege user with network denied except during consented model installation. Every recording, exposure, export, and deletion requires fresh consent."
            }));
        }
        if method == "ping" {
            return Ok(json!({}));
        }
        if self.lifecycle != LifecycleState::Initialized {
            return Err(McpError::InvalidRequest);
        }
        match method {
            "tools/list" => Ok(json!({"tools": tool_descriptors()})),
            "tools/call" => {
                let params = params.ok_or(McpError::InvalidParams)?;
                Ok(self
                    .call_tool(params)
                    .unwrap_or_else(|error| tool_error(&error)))
            },
            "resources/list" => Ok(json!({"resources": self.resources()})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": resource_templates()})),
            "resources/read" => self.read_resource(params.ok_or(McpError::InvalidParams)?),
            _ => Err(McpError::MethodNotFound),
        }
    }

    fn call_tool(
        &mut self,
        params: &Value,
    ) -> Result<Value, McpError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or(McpError::InvalidParams)?;
        let arguments = params.get("arguments").unwrap_or(&Value::Null);
        let value = match name {
            "koe_capabilities" => serde_json::to_value(
                self.backend
                    .capabilities()
                    .map_err(|_| McpError::OperationFailed)?,
            )
            .map_err(|_| McpError::OperationFailed)?,
            "koe_list_devices" => {
                let source = match string_arg(arguments, "source")? {
                    "microphone" | "mic" => SourceKind::Microphone,
                    "system" => SourceKind::System,
                    _ => return Err(McpError::InvalidParams),
                };
                serde_json::to_value(
                    self.backend
                        .enumerate(source)
                        .map_err(|_| McpError::OperationFailed)?,
                )
                .map_err(|_| McpError::OperationFailed)?
            },
            "koe_list_models" => {
                let manager = self.model_manager();
                serde_json::to_value(
                    manager
                        .inspect_installed_models_sync()
                        .map_err(|_| McpError::OperationFailed)?,
                )
                .map_err(|_| McpError::OperationFailed)?
            },
            "koe_install_model" => self.start_model_install(arguments)?,
            "koe_start_recording" => self.start_recording(arguments)?,
            "koe_stop_recording" => self.stop_recording(arguments, false)?,
            "koe_cancel_operation" => self.stop_recording(arguments, true)?,
            "koe_get_operation" => self.get_operation(arguments)?,
            "koe_get_session" => {
                let id = self.authorize_session(arguments, false)?;
                self.session_value(&id.to_string())?
            },
            "koe_get_transcript" => {
                let id = self.authorize_session(arguments, true)?;
                self.transcript_value(&id.to_string())?
            },
            "koe_export_session" => self.export_session(arguments)?,
            "koe_delete_session" => self.delete_session(arguments)?,
            _ => return Err(McpError::MethodNotFound),
        };
        if matches!(name, "koe_install_model" | "koe_start_recording")
            && let Some(token) = params
                .get("_meta")
                .and_then(|meta| meta.get("progressToken"))
                .cloned()
        {
            if !matches!(token, Value::String(_) | Value::Number(_))
                || self
                    .operations
                    .values()
                    .any(|operation| operation.progress_token.as_ref() == Some(&token))
            {
                return Err(McpError::InvalidParams);
            }
            let id = value
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or(McpError::OperationFailed)?;
            self.operations
                .get_mut(id)
                .ok_or(McpError::OperationFailed)?
                .progress_token = Some(token);
        }
        tool_result(value)
    }

    fn start_model_install(
        &mut self,
        args: &Value,
    ) -> Result<Value, McpError> {
        require_consent(args)?;
        require_only_keys(args, &["selector", "consent"])?;
        if self.active_operations() != 0 {
            return Err(McpError::Capacity);
        }
        let selector_text = string_arg(args, "selector")?;
        if selector_text.len() > 256 {
            return Err(McpError::InvalidParams);
        }
        let selector = selector_text
            .parse::<ModelSelector>()
            .map_err(|_| McpError::InvalidParams)?;
        let manager = self.model_manager();
        let operation_id = OperationId::new();
        let selector_key = selector.key();
        let snapshot = Arc::new(Mutex::new(OperationSnapshot {
            operation_id,
            session_id: None,
            state: OperationState::Running,
            progress: 0,
            error_code: None,
        }));
        let thread_snapshot = Arc::clone(&snapshot);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let thread_cancellation = cancellation.clone();
        let task = thread::spawn(move || {
            let (progress, mut progress_rx) = tokio::sync::mpsc::channel(8);
            let options = InstallOptions {
                policy: NetworkPolicy::ModelInstallOnly,
                cancel: thread_cancellation.clone(),
                progress: Some(progress),
                expected_descriptor: None,
                force_redownload: false,
            };
            let outcome = block_on(async {
                let install_with_progress = async {
                    let install = manager.install(&selector, &options);
                    tokio::pin!(install);
                    loop {
                        tokio::select! {
                            result = &mut install => break result,
                            phase = progress_rx.recv() => {
                                let Some(phase) = phase else {
                                    break install.await;
                                };
                                if let Ok(mut state) = thread_snapshot.lock() {
                                    state.progress = model_progress_value(&phase);
                                }
                            },
                        }
                    }
                };
                run_with_deadline(
                    install_with_progress,
                    &thread_cancellation,
                    OPERATION_DEADLINE,
                    CANCELLATION_GRACE,
                )
                .await
            });
            if let Ok(mut state) = thread_snapshot.lock() {
                state.state = match outcome {
                    TimedOperation::Completed(Ok(_)) => {
                        state.progress = 100;
                        OperationState::Completed
                    },
                    TimedOperation::Completed(Err(ModelError::Cancelled))
                    | TimedOperation::Cancelled => OperationState::Cancelled,
                    TimedOperation::DeadlineExceeded => {
                        state.error_code = Some(OPERATION_DEADLINE_ERROR);
                        OperationState::Failed
                    },
                    TimedOperation::Completed(Err(error)) => {
                        state.error_code = Some(error.code());
                        OperationState::Failed
                    },
                };
            }
        });
        tracing::info!(%operation_id, model = %selector_key, "model install operation started");
        self.operations.insert(
            operation_id.to_string(),
            Operation {
                snapshot,
                recording_control: None,
                cancellation,
                task: Some(task),
                progress_token: None,
                last_notified_progress: 0,
            },
        );
        serde_json::to_value(self.operation_snapshot(&operation_id.to_string())?)
            .map_err(|_| McpError::OperationFailed)
    }

    fn model_manager(&self) -> Arc<KoeModelManager> {
        Arc::clone(&self.model_manager)
    }

    #[allow(clippy::too_many_lines)]
    fn start_recording(
        &mut self,
        args: &Value,
    ) -> Result<Value, McpError> {
        require_consent(args)?;
        require_only_keys(
            args,
            &[
                "device_id",
                "sample_rate",
                "channels",
                "max_duration_seconds",
                "consent",
            ],
        )?;
        if self.active_operations() != 0 {
            return Err(McpError::Capacity);
        }
        let device_id = string_arg(args, "device_id")?;
        if device_id.len() > 512 {
            return Err(McpError::InvalidParams);
        }
        let device_id = device_id.to_owned();
        let sample_rate = u32_arg(args, "sample_rate", 48_000)?;
        let channels = u16_arg(args, "channels", 1)?;
        let max_duration_seconds = u32_arg(args, "max_duration_seconds", 14_400)?;
        if !(8_000..=192_000).contains(&sample_rate)
            || !(1..=8).contains(&channels)
            || !(1..=86_400).contains(&max_duration_seconds)
        {
            return Err(McpError::InvalidParams);
        }
        let request = OpenSource {
            device_id: device_id.clone(),
            kind: SourceKind::Microphone,
            preferred_sample_rate: sample_rate,
            preferred_channels: channels,
            negotiation: koe_audio::FormatNegotiation::default(),
        };
        let mut stream = self
            .backend
            .open(&request)
            .map_err(|_| McpError::OperationFailed)?;
        let mut config =
            RecordingConfig::microphone(&self.data_root, stream.sample_rate(), stream.channels());
        config.source_device_id = device_id;
        "mcp-cpal".clone_into(&mut config.backend);
        "granted".clone_into(&mut config.permission_result);
        stream
            .native_sample_format()
            .manifest_label()
            .clone_into(&mut config.native_sample_format);
        let (coordinator, task) = RecorderCoordinator::spawn(config.clone());
        let started = coordinator
            .start(RecordingConsent {
                microphone: true,
                system_audio: false,
                storage: true,
            })
            .map_err(|_| McpError::OperationFailed)?;
        let session_id = started.session_id.ok_or(McpError::OperationFailed)?;
        let (producer, consumer) =
            frame_ring(config.queue_capacity, 16_384).map_err(|_| McpError::OperationFailed)?;
        if stream.start(Box::new(producer)).is_err() {
            let _ignored = coordinator.cancel();
            let _ignored = task.shutdown();
            return Err(McpError::OperationFailed);
        }
        coordinator
            .mark_recording()
            .map_err(|_| McpError::OperationFailed)?;
        let operation_id = started.operation_id;
        tracing::info!(
            %operation_id,
            %session_id,
            sample_rate,
            channels,
            "recording operation started"
        );
        let snapshot = Arc::new(Mutex::new(OperationSnapshot {
            operation_id,
            session_id: Some(session_id),
            state: OperationState::Running,
            progress: 1,
            error_code: None,
        }));
        let thread_snapshot = Arc::clone(&snapshot);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let thread_cancellation = cancellation.clone();
        let (control, receiver) = mpsc::channel();
        let task = thread::spawn(move || {
            let mut samples = vec![0_i16; 16_384];
            recording_loop(
                &mut stream,
                &consumer,
                &coordinator,
                task,
                &receiver,
                Duration::from_secs(u64::from(max_duration_seconds)),
                &thread_cancellation,
                &thread_snapshot,
                &mut samples,
            );
        });
        self.operations.insert(
            operation_id.to_string(),
            Operation {
                snapshot,
                recording_control: Some(control),
                cancellation,
                task: Some(task),
                progress_token: None,
                last_notified_progress: 0,
            },
        );
        serde_json::to_value(self.operation_snapshot(&operation_id.to_string())?)
            .map_err(|_| McpError::OperationFailed)
    }

    fn stop_recording(
        &self,
        args: &Value,
        cancel: bool,
    ) -> Result<Value, McpError> {
        require_consent(args)?;
        require_only_keys(args, &["operation_id", "consent"])?;
        let operation_id = string_arg(args, "operation_id")?;
        let operation = self
            .operations
            .get(operation_id)
            .ok_or(McpError::NotFound)?;
        let state = operation
            .snapshot
            .lock()
            .map_err(|_| McpError::OperationFailed)?
            .state
            .clone();
        if matches!(state, OperationState::Running) {
            tracing::info!(operation_id, cancel, "stop requested for operation");
            if let Some(control) = &operation.recording_control {
                if cancel {
                    operation.cancellation.cancel();
                }
                let command = if cancel {
                    RecordingControl::Cancel
                } else {
                    RecordingControl::Stop
                };
                control
                    .send(command)
                    .map_err(|_| McpError::OperationFailed)?;
            } else if cancel {
                operation.cancellation.cancel();
            } else {
                return Err(McpError::InvalidParams);
            }
        }
        serde_json::to_value(self.operation_snapshot(operation_id)?)
            .map_err(|_| McpError::OperationFailed)
    }

    fn get_operation(
        &self,
        args: &Value,
    ) -> Result<Value, McpError> {
        require_only_keys(args, &["operation_id"])?;
        serde_json::to_value(self.operation_snapshot(string_arg(args, "operation_id")?)?)
            .map_err(|_| McpError::OperationFailed)
    }

    fn operation_snapshot(
        &self,
        id: &str,
    ) -> Result<OperationSnapshot, McpError> {
        self.operations
            .get(id)
            .ok_or(McpError::NotFound)?
            .snapshot
            .lock()
            .map_err(|_| McpError::OperationFailed)
            .map(|snapshot| snapshot.clone())
    }

    fn active_operations(&self) -> usize {
        self.operations
            .values()
            .filter(|operation| {
                operation
                    .snapshot
                    .lock()
                    .is_ok_and(|s| matches!(s.state, OperationState::Running))
            })
            .count()
    }

    fn reap_operations(&mut self) {
        for operation in self.operations.values_mut() {
            let finished = operation.task.as_ref().is_some_and(JoinHandle::is_finished);
            if finished && let Some(task) = operation.task.take() {
                let joined = task.join();
                if let Ok(mut snapshot) = operation.snapshot.lock()
                    && matches!(snapshot.state, OperationState::Running)
                {
                    let error_code = if joined.is_err() {
                        "KOE-SESSION-WORKER-PANICKED"
                    } else {
                        "KOE-SESSION-WORKER-STOPPED"
                    };
                    tracing::warn!(
                        operation_id = %snapshot.operation_id,
                        error_code,
                        "operation worker stopped without a terminal state"
                    );
                    snapshot.state = OperationState::Failed;
                    snapshot.error_code = Some(error_code);
                }
            }
            let terminal = operation
                .snapshot
                .lock()
                .is_ok_and(|snapshot| !matches!(snapshot.state, OperationState::Running));
            if terminal && let Some(task) = operation.task.take() {
                let _ignored = task.join();
            }
        }
        while self.operations.len() > MAX_RETAINED_OPERATIONS {
            let removable = self.operations.iter().find_map(|(id, operation)| {
                operation
                    .snapshot
                    .lock()
                    .is_ok_and(|snapshot| !matches!(snapshot.state, OperationState::Running))
                    .then(|| id.clone())
            });
            let Some(id) = removable else {
                break;
            };
            self.operations.remove(&id);
        }
    }

    fn emit_progress(
        &mut self,
        output: &mut impl Write,
    ) -> io::Result<()> {
        for operation in self.operations.values_mut() {
            let Some(token) = operation.progress_token.clone() else {
                continue;
            };
            let Ok(snapshot) = operation.snapshot.lock() else {
                continue;
            };
            let progress = snapshot.progress;
            let terminal = !matches!(snapshot.state, OperationState::Running);
            let state = format!("{:?}", snapshot.state).to_lowercase();
            drop(snapshot);
            if progress > operation.last_notified_progress || terminal {
                write_message(
                    output,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {"progressToken": token, "progress": progress, "total": 100, "message": state}
                    }),
                )?;
                operation.last_notified_progress = progress;
            }
            if terminal {
                operation.progress_token = None;
            }
        }
        Ok(())
    }

    fn resources(&self) -> Vec<Value> {
        let mut resources = vec![json!({
            "uri": "koe://capabilities", "name": "Audio capabilities", "mimeType": "application/json"
        })];
        for uri in &self.authorized_resources {
            resources.push(json!({"uri": uri, "name": "Authorized session data", "mimeType": "application/json"}));
        }
        for id in self.operations.keys() {
            resources.push(json!({"uri": format!("koe://operations/{id}"), "name": format!("Operation {id}"), "mimeType": "application/json"}));
        }
        resources
    }

    fn read_resource(
        &self,
        params: &Value,
    ) -> Result<Value, McpError> {
        let uri = string_arg(params, "uri")?;
        let value = if uri == "koe://capabilities" {
            serde_json::to_value(
                self.backend
                    .capabilities()
                    .map_err(|_| McpError::OperationFailed)?,
            )
            .map_err(|_| McpError::OperationFailed)?
        } else if let Some(id) = uri.strip_prefix("koe://operations/") {
            serde_json::to_value(self.operation_snapshot(id)?)
                .map_err(|_| McpError::OperationFailed)?
        } else if let Some(rest) = uri.strip_prefix("koe://sessions/") {
            let id_text = rest.strip_suffix("/transcript").unwrap_or(rest);
            let _id = SessionId::parse(id_text).map_err(|_| McpError::Unauthorized)?;
            if !self.authorized_resources.contains(uri) {
                return Err(McpError::Unauthorized);
            }
            if rest.ends_with("/transcript") {
                self.transcript_value(id_text)?
            } else {
                self.session_value(id_text)?
            }
        } else {
            return Err(McpError::NotFound);
        };
        let text = serde_json::to_string(&value).map_err(|_| McpError::OperationFailed)?;
        Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}))
    }

    fn authorize_session(
        &mut self,
        args: &Value,
        transcript: bool,
    ) -> Result<SessionId, McpError> {
        require_consent(args)?;
        require_only_keys(args, &["session_id", "consent"])?;
        let id = SessionId::parse(string_arg(args, "session_id")?)
            .map_err(|_| McpError::Unauthorized)?;
        let _verified = self.session_dir(id)?;
        let suffix = if transcript { "/transcript" } else { "" };
        self.authorized_resources
            .insert(format!("koe://sessions/{id}{suffix}"));
        Ok(id)
    }

    fn session_value(
        &self,
        id: &str,
    ) -> Result<Value, McpError> {
        let session = SessionId::parse(id).map_err(|_| McpError::Unauthorized)?;
        let directory = self.session_dir(session)?;
        let path = directory.join("session.json");
        let canonical = path.canonicalize().map_err(|_| McpError::NotFound)?;
        if canonical.parent() != Some(directory.as_path()) {
            return Err(McpError::Unauthorized);
        }
        let bytes = read_limited_file(&canonical, MAX_OUTPUT_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|_| McpError::OperationFailed)
    }

    fn transcript_value(
        &self,
        id: &str,
    ) -> Result<Value, McpError> {
        let session = SessionId::parse(id).map_err(|_| McpError::Unauthorized)?;
        let directory = self.session_dir(session)?;
        let path = directory.join("transcript/final.json");
        let canonical = path.canonicalize().map_err(|_| McpError::NotFound)?;
        if !canonical.starts_with(&directory) {
            return Err(McpError::Unauthorized);
        }
        let bytes = read_limited_file(&canonical, MAX_OUTPUT_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|_| McpError::OperationFailed)
    }

    fn export_session(
        &self,
        args: &Value,
    ) -> Result<Value, McpError> {
        require_consent(args)?;
        require_only_keys(args, &["session_id", "consent"])?;
        let root = self.export_root.as_ref().ok_or(McpError::Unauthorized)?;
        let id = SessionId::parse(string_arg(args, "session_id")?)
            .map_err(|_| McpError::Unauthorized)?;
        let source = self.session_dir(id)?;
        let manifest = read_manifest(&source)?;
        if !manifest.state.is_terminal() {
            return Err(McpError::Unauthorized);
        }
        let export_name = format!("{id}-export");
        let destination = root.join(&export_name);
        if destination.exists() {
            return Err(McpError::Unauthorized);
        }
        let staging = root.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        fs::create_dir(&staging).map_err(|_| McpError::Unauthorized)?;
        set_owner_only(&staging)?;
        if let Err(error) = copy_tree(&source, &staging) {
            let _ignored = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if fs::rename(&staging, &destination).is_err() {
            let _ignored = fs::remove_dir_all(&staging);
            return Err(McpError::OperationFailed);
        }
        Ok(json!({"session_id": id, "export_name": export_name}))
    }

    fn delete_session(
        &self,
        args: &Value,
    ) -> Result<Value, McpError> {
        require_consent(args)?;
        require_only_keys(args, &["session_id", "consent"])?;
        let id = SessionId::parse(string_arg(args, "session_id")?)
            .map_err(|_| McpError::Unauthorized)?;
        let path = self.session_dir(id)?;
        let manifest = read_manifest(&path)?;
        if !manifest.state.is_terminal() {
            return Err(McpError::Unauthorized);
        }
        fs::remove_dir_all(path).map_err(|_| McpError::OperationFailed)?;
        Ok(json!({"session_id": id, "deleted": true}))
    }

    fn session_dir(
        &self,
        id: SessionId,
    ) -> Result<PathBuf, McpError> {
        let path = self.sessions_root.join(id.to_string());
        let canonical = path.canonicalize().map_err(|_| McpError::NotFound)?;
        if canonical.parent() != Some(self.sessions_root.as_path())
            || fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(McpError::Unauthorized);
        }
        Ok(canonical)
    }

    fn cancel_all(&mut self) {
        for operation in self.operations.values() {
            if let Some(control) = &operation.recording_control {
                let _ignored = control.send(RecordingControl::Cancel);
            }
            operation.cancellation.cancel();
        }
        for operation in self.operations.values_mut() {
            if let Some(task) = operation.task.take() {
                let _ignored = task.join();
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn recording_loop<S: AudioStream>(
    stream: &mut S,
    consumer: &koe_audio::FrameConsumer,
    coordinator: &RecorderCoordinator,
    task: koe_app::CoordinatorTask,
    controls: &Receiver<RecordingControl>,
    max_duration: Duration,
    cancellation: &tokio_util::sync::CancellationToken,
    snapshot: &Arc<Mutex<OperationSnapshot>>,
    samples: &mut [i16],
) {
    let mut terminal = None;
    let mut failure_code = None;
    let mut callback_anchor_ns = None;
    let mut capture_epoch_id = 0_u64;
    let deadline = std::time::Instant::now() + max_duration;
    'capture: loop {
        if std::time::Instant::now() >= deadline {
            terminal = Some(true);
            break;
        }
        if cancellation.is_cancelled() {
            terminal = Some(true);
            break;
        }
        if let Ok(control) = controls.try_recv() {
            terminal =
                Some(cancellation.is_cancelled() || matches!(control, RecordingControl::Cancel));
            break;
        }
        if let Some(failure) = consumer.take_runtime_failure() {
            failure_code = Some(failure.audio_error().code());
            break;
        }
        if consumer.take_device_lost() {
            failure_code = Some("KOE-AUDIO-DEVICE-LOST");
            break;
        }
        let discontinuities = consumer.take_discontinuities();
        if discontinuities != 0 {
            capture_epoch_id = capture_epoch_id.saturating_add(discontinuities);
            if coordinator
                .record_discontinuity(consumer.discontinuity_timestamp_ns())
                .is_err()
            {
                failure_code = Some("KOE-STORE-FINALIZE-FAILED");
                break;
            }
        }
        let dropped = consumer.take_dropped_frames();
        if dropped != 0
            && coordinator
                .record_overflow(TrackKind::Microphone, dropped)
                .is_err()
        {
            failure_code = Some("KOE-STORE-FINALIZE-FAILED");
            break;
        }
        match consumer.try_pop(samples) {
            Ok(Some(metadata)) => {
                if let Some(failure) = metadata.runtime_failure {
                    failure_code = Some(failure.audio_error().code());
                    break 'capture;
                }
                if metadata.device_lost {
                    failure_code = Some("KOE-AUDIO-DEVICE-LOST");
                    break 'capture;
                }
                if metadata.overflow {
                    if coordinator
                        .record_overflow(TrackKind::Microphone, metadata.dropped_frames.max(1))
                        .is_err()
                    {
                        failure_code = Some("KOE-STORE-FINALIZE-FAILED");
                        break 'capture;
                    }
                    continue;
                }
                let count = usize::try_from(metadata.sample_count)
                    .unwrap_or(0)
                    .min(samples.len());
                let anchor =
                    *callback_anchor_ns.get_or_insert(metadata.callback_arrival_timestamp_ns);
                if metadata.discontinuity {
                    capture_epoch_id = capture_epoch_id.saturating_add(1);
                }
                let channels = u64::from(metadata.channels.max(1));
                let timeline = TimelineBlock {
                    session_start_us: metadata
                        .callback_arrival_timestamp_ns
                        .saturating_sub(anchor)
                        / 1_000,
                    capture_epoch_id,
                    source_capture_start_ns: metadata.capture_timestamp_ns,
                    callback_arrival_ns: metadata.callback_arrival_timestamp_ns,
                    sequence: metadata.sequence,
                    frame_count: u64::try_from(count).unwrap_or(u64::MAX) / channels,
                    discontinuity_before: metadata.discontinuity,
                };
                if let Err(error) = coordinator.append_block(samples[..count].to_vec(), timeline) {
                    failure_code = Some(error.code());
                    break 'capture;
                }
            },
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                failure_code = Some(error.code());
                break;
            },
        }
    }
    let stream_failure = stream.stop().err().map(koe_audio::AudioError::code);
    let result = failure_code.map_or_else(
        || {
            if terminal == Some(false) {
                coordinator.stop()
            } else {
                coordinator.cancel()
            }
        },
        |code| coordinator.fail(code),
    );
    let shutdown_failure = task.shutdown().err().map(|error| error.code());
    let terminal_failure = failure_code.or(stream_failure).or(shutdown_failure);
    if let Ok(mut state) = snapshot.lock() {
        state.progress = 100;
        if let Some(code) = terminal_failure {
            state.state = OperationState::Failed;
            state.error_code = Some(code);
        } else {
            match result {
                Ok(final_state) => {
                    state.state = if final_state.state == SessionState::Completed {
                        OperationState::Completed
                    } else {
                        OperationState::Cancelled
                    };
                },
                Err(error) => {
                    state.state = OperationState::Failed;
                    state.error_code = Some(error.code());
                },
            }
        }
    }
}

enum Frame {
    Message(String),
    Oversized,
}

fn read_frame(input: &mut impl BufRead) -> io::Result<Option<Frame>> {
    let mut bytes = Vec::with_capacity(8 * 1024);
    let mut oversized = false;
    let mut saw_input = false;
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return if saw_input {
                Ok(Some(if oversized {
                    Frame::Oversized
                } else {
                    Frame::Message(String::from_utf8(bytes).unwrap_or_default())
                }))
            } else {
                Ok(None)
            };
        }
        saw_input = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if !oversized {
            if bytes.len().saturating_add(count) > MAX_REQUEST_BYTES {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..count]);
            }
        }
        input.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(if oversized {
                Frame::Oversized
            } else {
                Frame::Message(String::from_utf8(bytes).unwrap_or_default())
            }));
        }
    }
}

fn authorize_root(
    path: &Path,
    create: bool,
) -> Result<PathBuf, McpError> {
    if !create && !path.exists() {
        return Err(McpError::Unauthorized);
    }
    if create {
        fs::create_dir_all(path).map_err(|_| McpError::Unauthorized)?;
    }
    koe_recording::secure_app_directory(path).map_err(|_| McpError::Unauthorized)?;
    set_owner_only(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| McpError::Unauthorized)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !is_owner_only(&metadata) {
        return Err(McpError::Unauthorized);
    }
    path.canonicalize().map_err(|_| McpError::Unauthorized)
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<(), McpError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| McpError::Unauthorized)
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<(), McpError> {
    Ok(())
}

#[cfg(unix)]
fn is_owner_only(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode().trailing_zeros() >= 6
}

#[cfg(not(unix))]
const fn is_owner_only(_metadata: &fs::Metadata) -> bool {
    true
}

fn read_limited_file(
    path: &Path,
    limit: usize,
) -> Result<Vec<u8>, McpError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| McpError::NotFound)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || has_multiple_links(&metadata) {
        return Err(McpError::Unauthorized);
    }
    if metadata.len() > u64::try_from(limit).unwrap_or(u64::MAX) {
        return Err(McpError::ResponseTooLarge);
    }
    let file = fs::File::open(path).map_err(|_| McpError::NotFound)?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(limit).min(limit));
    file.take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| McpError::OperationFailed)?;
    if bytes.len() > limit {
        return Err(McpError::ResponseTooLarge);
    }
    Ok(bytes)
}

fn read_manifest(path: &Path) -> Result<SessionManifest, McpError> {
    serde_json::from_slice(&read_limited_file(
        &path.join("session.json"),
        MAX_OUTPUT_BYTES,
    )?)
    .map_err(|_| McpError::OperationFailed)
}

fn copy_tree(
    source: &Path,
    destination: &Path,
) -> Result<(), McpError> {
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    copy_tree_inner(source, destination, 0, &mut files, &mut bytes)
}

fn copy_tree_inner(
    source: &Path,
    destination: &Path,
    depth: usize,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<(), McpError> {
    if depth > MAX_EXPORT_DEPTH {
        return Err(McpError::Capacity);
    }
    for entry in fs::read_dir(source).map_err(|_| McpError::OperationFailed)? {
        let entry = entry.map_err(|_| McpError::OperationFailed)?;
        let kind = entry.file_type().map_err(|_| McpError::OperationFailed)?;
        let metadata = entry.metadata().map_err(|_| McpError::OperationFailed)?;
        if kind.is_symlink() || (kind.is_file() && has_multiple_links(&metadata)) {
            return Err(McpError::Unauthorized);
        }
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&target).map_err(|_| McpError::OperationFailed)?;
            copy_tree_inner(
                &entry.path(),
                &target,
                depth.saturating_add(1),
                files,
                bytes,
            )?;
        } else if kind.is_file() {
            *files = files.saturating_add(1);
            *bytes = bytes.saturating_add(metadata.len());
            if *files > MAX_EXPORT_FILES || *bytes > MAX_EXPORT_BYTES {
                return Err(McpError::Capacity);
            }
            fs::copy(entry.path(), target).map_err(|_| McpError::OperationFailed)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn has_multiple_links(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() > 1
}

#[cfg(not(unix))]
const fn has_multiple_links(_metadata: &fs::Metadata) -> bool {
    false
}

enum TimedOperation<T> {
    Completed(T),
    Cancelled,
    DeadlineExceeded,
}

#[derive(Clone, Copy)]
enum StopReason {
    Cancelled,
    DeadlineExceeded,
}

async fn run_with_deadline<F>(
    operation: F,
    cancellation: &tokio_util::sync::CancellationToken,
    deadline: Duration,
    cancellation_grace: Duration,
) -> TimedOperation<F::Output>
where
    F: Future,
{
    tokio::pin!(operation);
    let reason = tokio::select! {
        biased;
        output = &mut operation => return TimedOperation::Completed(output),
        () = cancellation.cancelled() => StopReason::Cancelled,
        () = tokio::time::sleep(deadline) => StopReason::DeadlineExceeded,
    };
    cancellation.cancel();
    if let Ok(output) = tokio::time::timeout(cancellation_grace, &mut operation).await {
        return TimedOperation::Completed(output);
    }
    match reason {
        StopReason::Cancelled => TimedOperation::Cancelled,
        StopReason::DeadlineExceeded => TimedOperation::DeadlineExceeded,
    }
}

const fn model_progress_value(progress: &ModelProgress) -> u8 {
    match progress {
        ModelProgress::Resolving => 5,
        ModelProgress::Downloading => 30,
        ModelProgress::Verifying => 70,
        ModelProgress::Installing => 90,
        ModelProgress::Done => 100,
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_or_else(
            |_| panic_free_future_failure(),
            |runtime| runtime.block_on(future),
        )
}

fn panic_free_future_failure<T>() -> T {
    // This path cannot manufacture an arbitrary future output; runtime creation
    // is expected to be infallible in supported environments. Abort preserves
    // stdout cleanliness and triggers process-owner orphan cleanup.
    std::process::abort()
}

fn require_only_keys(
    args: &Value,
    allowed: &[&str],
) -> Result<(), McpError> {
    let object = args.as_object().ok_or(McpError::InvalidParams)?;
    if object.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(McpError::InvalidParams)
    }
}

fn require_consent(args: &Value) -> Result<(), McpError> {
    if args.get("consent").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(McpError::ConsentRequired)
    }
}

fn string_arg<'a>(
    args: &'a Value,
    name: &str,
) -> Result<&'a str, McpError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or(McpError::InvalidParams)
}

fn u32_arg(
    args: &Value,
    name: &str,
    default: u32,
) -> Result<u32, McpError> {
    args.get(name).map_or(Ok(default), |value| {
        value
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .ok_or(McpError::InvalidParams)
    })
}

fn u16_arg(
    args: &Value,
    name: &str,
    default: u16,
) -> Result<u16, McpError> {
    args.get(name).map_or(Ok(default), |value| {
        value
            .as_u64()
            .and_then(|number| u16::try_from(number).ok())
            .ok_or(McpError::InvalidParams)
    })
}

#[allow(clippy::needless_pass_by_value)]
fn tool_error(error: &McpError) -> Value {
    let value = json!({"code": error.koe_code(), "message": error.to_string()});
    json!({
        "content": [{"type": "text", "text": value.to_string()}],
        "structuredContent": value,
        "isError": true
    })
}

fn resource_templates() -> Vec<Value> {
    vec![
        json!({"uriTemplate": "koe://operations/{id}", "name": "Authorized operation", "mimeType": "application/json"}),
        json!({"uriTemplate": "koe://sessions/{id}", "name": "Authorized session", "mimeType": "application/json"}),
        json!({"uriTemplate": "koe://sessions/{id}/transcript", "name": "Authorized transcript", "mimeType": "application/json"}),
    ]
}

#[allow(clippy::needless_pass_by_value)]
fn tool_result(value: Value) -> Result<Value, McpError> {
    let text = serde_json::to_string(&value).map_err(|_| McpError::OperationFailed)?;
    if text.len() > MAX_OUTPUT_BYTES {
        return Err(McpError::ResponseTooLarge);
    }
    Ok(
        json!({"content": [{"type": "text", "text": text}], "structuredContent": value, "isError": false}),
    )
}

fn write_message(
    output: &mut impl Write,
    message: &Value,
) -> io::Result<()> {
    let encoded = match serde_json::to_vec(message) {
        Ok(encoded) if encoded.len() <= MAX_OUTPUT_BYTES => encoded,
        _ => serde_json::to_vec(&error_response(
            message.get("id").cloned().unwrap_or(Value::Null),
            &McpError::ResponseTooLarge,
        ))
        .unwrap_or_else(|_| b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"internal error\"}}".to_vec()),
    };
    output.write_all(&encoded)?;
    output.write_all(b"\n")?;
    output.flush()
}

#[allow(clippy::needless_pass_by_value)]
fn error_response(
    id: Value,
    error: &McpError,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": error.rpc_code(), "message": error.to_string(), "data": {"koe_code": error.koe_code()}}
    })
}

fn tool_descriptors() -> Vec<Value> {
    let object = || json!({"type": "object", "additionalProperties": false, "properties": {}});
    vec![
        tool(
            "koe_capabilities",
            "List runtime audio capabilities without prompting",
            object(),
        ),
        tool(
            "koe_list_devices",
            "List explicitly selected source devices",
            json!({"type":"object","additionalProperties":false,"required":["source"],"properties":{"source":{"enum":["microphone","system"]}}}),
        ),
        tool(
            "koe_list_models",
            "List locally installed models; never accesses the network",
            object(),
        ),
        tool(
            "koe_install_model",
            "Install a model (disabled unless an installation policy is configured)",
            json!({"type":"object","additionalProperties":false,"required":["selector","consent"],"properties":{"selector":{"type":"string","maxLength":256},"consent":{"const":true}}}),
        ),
        tool(
            "koe_start_recording",
            "Start microphone recording; requires fresh host consent",
            json!({"type":"object","additionalProperties":false,"required":["device_id","consent"],"properties":{"device_id":{"type":"string","maxLength":512},"sample_rate":{"type":"integer","minimum":8000,"maximum":192_000},"channels":{"type":"integer","minimum":1,"maximum":8},"max_duration_seconds":{"type":"integer","minimum":1,"maximum":86_400},"consent":{"const":true}}}),
        ),
        tool(
            "koe_stop_recording",
            "Stop and finalize a recording; requires consent",
            operation_schema(),
        ),
        tool(
            "koe_cancel_operation",
            "Idempotently cancel and finalize a recording",
            operation_schema(),
        ),
        tool(
            "koe_get_operation",
            "Read operation progress and terminal state",
            json!({"type":"object","additionalProperties":false,"required":["operation_id"],"properties":{"operation_id":{"type":"string","format":"uuid"}}}),
        ),
        sensitive_session_tool("koe_get_session", "Read one authorized session manifest"),
        sensitive_session_tool("koe_get_transcript", "Expose one authorized transcript"),
        sensitive_session_tool(
            "koe_export_session",
            "Export one authorized terminal session below the configured export root",
        ),
        sensitive_session_tool(
            "koe_delete_session",
            "Delete one authorized terminal session; filesystem erasure is not guaranteed",
        ),
    ]
}

#[allow(clippy::needless_pass_by_value)]
fn tool(
    name: &str,
    description: &str,
    input_schema: Value,
) -> Value {
    json!({"name": name, "description": description, "inputSchema": input_schema})
}

fn operation_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["operation_id","consent"],"properties":{"operation_id":{"type":"string","format":"uuid"},"consent":{"const":true}}})
}

fn sensitive_session_tool(
    name: &str,
    description: &str,
) -> Value {
    tool(
        name,
        description,
        json!({"type":"object","additionalProperties":false,"required":["session_id","consent"],"properties":{"session_id":{"type":"string","format":"uuid"},"consent":{"const":true}}}),
    )
}

/// Installs a human-readable tracing subscriber on stderr. stdout stays
/// reserved exclusively for JSON-RPC frames. The level defaults to `info`
/// and can be overridden with `RUST_LOG`.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .compact()
        .init();
}

fn main() -> std::process::ExitCode {
    init_tracing();
    let args = Args::parse();
    let mut server = match Server::new(&args) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("{}: {error}", error.koe_code());
            return std::process::ExitCode::FAILURE;
        },
    };
    tracing::info!(
        data_root = %server.data_root.display(),
        "koe-mcp server started"
    );
    match server.run(io::BufReader::new(io::stdin()), io::stdout().lock()) {
        Ok(()) => {
            tracing::info!("koe-mcp server stopped");
            std::process::ExitCode::SUCCESS
        },
        Err(error) => {
            eprintln!("KOE-MCP-STDIO-FAILED: {error}");
            std::process::ExitCode::FAILURE
        },
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use std::{
        future::pending,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use koe_core::{OperationId, SessionId};

    use super::{
        Args, LifecycleState, Operation, OperationSnapshot, OperationState, Server, TimedOperation,
        run_with_deadline,
    };

    fn request(
        server: &mut Server,
        input: &str,
    ) -> serde_json::Value {
        let mut output = Vec::new();
        server.run(input.as_bytes(), &mut output).expect("run");
        serde_json::from_slice(output.strip_suffix(b"\n").expect("newline")).expect("json")
    }

    fn server(root: &TempDir) -> Server {
        Server::new(&Args {
            data_root: root.path().to_path_buf(),
            export_root: None,
        })
        .expect("server")
    }

    fn initialized_server(root: &TempDir) -> Server {
        let mut server = server(root);
        server.lifecycle = LifecycleState::Initialized;
        server
    }

    #[tokio::test]
    async fn operation_deadline_has_bounded_cooperative_cancellation() {
        let cooperative_cancel = tokio_util::sync::CancellationToken::new();
        let observed_cancel = cooperative_cancel.clone();
        let cooperative = async move {
            observed_cancel.cancelled().await;
            7_u8
        };
        assert!(matches!(
            run_with_deadline(
                cooperative,
                &cooperative_cancel,
                Duration::ZERO,
                Duration::from_secs(1),
            )
            .await,
            TimedOperation::Completed(7)
        ));
        assert!(cooperative_cancel.is_cancelled());

        let ignored_cancel = tokio_util::sync::CancellationToken::new();
        assert!(matches!(
            run_with_deadline(
                pending::<()>(),
                &ignored_cancel,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await,
            TimedOperation::DeadlineExceeded
        ));
        assert!(ignored_cancel.is_cancelled());

        let explicit_cancel = tokio_util::sync::CancellationToken::new();
        explicit_cancel.cancel();
        assert!(matches!(
            run_with_deadline(
                pending::<()>(),
                &explicit_cancel,
                Duration::from_secs(1),
                Duration::ZERO,
            )
            .await,
            TimedOperation::Cancelled
        ));
    }

    #[test]
    fn initialize_and_tools_are_protocol_only_json() {
        let root = TempDir::new().expect("temp");
        let mut server = server(&root);
        let response = request(
            &mut server,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
        );
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn sensitive_tool_requires_fresh_consent() {
        let root = TempDir::new().expect("temp");
        let mut server = initialized_server(&root);
        let response = request(
            &mut server,
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"koe_delete_session\",\"arguments\":{\"session_id\":\"00000000-0000-0000-0000-000000000000\"}}}\n",
        );
        assert_eq!(
            response["result"]["structuredContent"]["code"],
            "KOE-POLICY-CONSENT-REQUIRED"
        );
    }

    #[test]
    fn traversal_is_rejected_as_an_invalid_session() {
        let root = TempDir::new().expect("temp");
        let mut server = initialized_server(&root);
        let response = request(
            &mut server,
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"koe_get_session\",\"arguments\":{\"session_id\":\"../../etc\",\"consent\":true}}}\n",
        );
        assert_eq!(
            response["result"]["structuredContent"]["code"],
            "KOE-STORE-PATH-REJECTED"
        );
    }

    #[test]
    fn oversized_request_is_rejected_without_parsing() {
        let root = TempDir::new().expect("temp");
        let mut server = server(&root);
        let input = format!("{}\n", "x".repeat(super::MAX_REQUEST_BYTES + 1));
        let response = request(&mut server, &input);
        assert_eq!(
            response["error"]["data"]["koe_code"],
            "KOE-MCP-REQUEST-TOO-LARGE"
        );
    }

    #[test]
    fn session_consent_does_not_authorize_transcript_resource() {
        let root = TempDir::new().expect("temp");
        let mut server = initialized_server(&root);
        let id = SessionId::new();
        let directory = root.path().join("sessions").join(id.to_string());
        std::fs::create_dir_all(directory.join("transcript")).expect("session directories");
        std::fs::write(directory.join("session.json"), "{}").expect("manifest");
        std::fs::write(directory.join("transcript/final.json"), "[]").expect("transcript");
        server
            .call_tool(&serde_json::json!({
                "name": "koe_get_session",
                "arguments": {"session_id": id, "consent": true}
            }))
            .expect("authorize manifest");
        let error = server
            .read_resource(&serde_json::json!({
                "uri": format!("koe://sessions/{id}/transcript")
            }))
            .expect_err("transcript needs its own authorization");
        assert_eq!(error.koe_code(), "KOE-STORE-PATH-REJECTED");
    }

    #[test]
    fn progress_notifications_are_monotonic_and_stop_at_terminal() {
        let root = TempDir::new().expect("temp");
        let mut server = initialized_server(&root);
        let id = OperationId::new();
        let snapshot = Arc::new(Mutex::new(OperationSnapshot {
            operation_id: id,
            session_id: None,
            state: OperationState::Running,
            progress: 25,
            error_code: None,
        }));
        server.operations.insert(
            id.to_string(),
            Operation {
                snapshot: Arc::clone(&snapshot),
                recording_control: None,
                cancellation: tokio_util::sync::CancellationToken::new(),
                task: None,
                progress_token: Some(serde_json::json!("progress-1")),
                last_notified_progress: 0,
            },
        );
        let mut output = Vec::new();
        server.emit_progress(&mut output).expect("running progress");
        {
            let mut state = snapshot.lock().expect("snapshot");
            state.progress = 100;
            state.state = OperationState::Completed;
        }
        server
            .emit_progress(&mut output)
            .expect("terminal progress");
        let terminal_len = output.len();
        server
            .emit_progress(&mut output)
            .expect("no post-terminal progress");
        assert_eq!(output.len(), terminal_len);
        let messages = String::from_utf8(output).expect("utf8");
        let progress = messages
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json"))
            .map(|message| message["params"]["progress"].as_u64().expect("progress"))
            .collect::<Vec<_>>();
        assert_eq!(progress, vec![25, 100]);
    }

    #[test]
    fn cancelling_a_terminal_operation_is_idempotent() {
        let root = TempDir::new().expect("temp");
        let mut server = server(&root);
        let id = OperationId::new();
        server.operations.insert(
            id.to_string(),
            Operation {
                snapshot: Arc::new(Mutex::new(OperationSnapshot {
                    operation_id: id,
                    session_id: None,
                    state: OperationState::Completed,
                    progress: 100,
                    error_code: None,
                })),
                recording_control: None,
                cancellation: tokio_util::sync::CancellationToken::new(),
                task: None,
                progress_token: None,
                last_notified_progress: 0,
            },
        );
        let arguments = serde_json::json!({"operation_id": id, "consent": true});
        server
            .stop_recording(&arguments, true)
            .expect("first cancel");
        server
            .stop_recording(&arguments, true)
            .expect("second cancel");
        assert!(matches!(
            server
                .operation_snapshot(&id.to_string())
                .expect("snapshot")
                .state,
            OperationState::Completed
        ));
    }
}
