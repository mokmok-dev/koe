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
use koe_audio::{
    AudioBackend, AudioStream, CanonicalNormalizer, CpalBackend, OpenSource, frame_ring,
};
use koe_core::{NetworkPolicy, OperationId, SessionId, SessionState, SourceKind};
use koe_model::{
    AsrSessionSettings, DigestAllowlist, FoundryLocalAdapter, InstallOptions, KoeModelManager,
    ModelError, ModelManager, ModelProgress, ModelSelector,
};
use koe_recording::{RecordingConfig, SessionManifest, TimelineBlock, TrackKind};
use koe_transcript::{
    SegmentId, TranscriptModel, TranscriptSegment, TranscriptSegmentState, TranscriptStore,
};
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OperationState {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct OperationSnapshot {
    operation_id: OperationId,
    session_id: Option<SessionId>,
    state: OperationState,
    progress: u8,
    error_code: Option<String>,
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
    #[error("session not found")]
    SessionNotFound,
    #[error("model not found")]
    ModelNotFound,
    #[error("operation not found")]
    OperationNotFound,
    #[error("resource not found")]
    ResourceNotFound,
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
            Self::SessionNotFound
            | Self::ModelNotFound
            | Self::OperationNotFound
            | Self::ResourceNotFound => -32004,
            Self::ConsentRequired | Self::Unauthorized => -32003,
            Self::Capacity => -32008,
            Self::OperationFailed | Self::ResponseTooLarge => -32603,
        }
    }

    const fn koe_code(&self) -> &'static str {
        match self {
            Self::ConsentRequired => "KOE-POLICY-CONSENT-REQUIRED",
            Self::Unauthorized => "KOE-STORE-PATH-REJECTED",
            Self::SessionNotFound => "KOE-SESSION-NOT-FOUND",
            Self::ModelNotFound => "KOE-MODEL-NOT-FOUND",
            Self::OperationNotFound => "KOE-MCP-OPERATION-NOT-FOUND",
            Self::ResourceNotFound => "KOE-MCP-RESOURCE-NOT-FOUND",
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

    const fn remedy(&self) -> &'static str {
        match self {
            Self::ConsentRequired => "repeat the tool call with explicit consent after user review",
            Self::Unauthorized => "request and authorize an app-owned session resource first",
            Self::SessionNotFound => "list sessions and retry with an existing session ID",
            Self::ModelNotFound => "install the model explicitly, then retry offline",
            Self::OperationNotFound => "start an operation and retry with its operation ID",
            Self::ResourceNotFound => "list resources and retry with an advertised URI",
            Self::Capacity => "wait for a running operation to finish, then retry",
            Self::InvalidParams | Self::InvalidRequest | Self::ParseError => {
                "correct the request using the advertised tool schema"
            },
            Self::MethodNotFound => "use a method advertised during MCP initialization",
            _ => "poll the operation resource and retry after correcting the stable error code",
        }
    }

    const fn retryable(&self) -> bool {
        // A generic operation failure covers validation, storage corruption,
        // and permanent runtime errors as well as transient failures. Claiming
        // all of those are retryable causes blind retry loops; only the typed
        // capacity condition is unambiguously transient here.
        matches!(self, Self::Capacity)
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
    operations_root: PathBuf,
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
        let operations_root = authorize_root(&data_root.join("operations"), true)?;
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
        // Reconcile abandoned recording manifests before reconstructing their
        // operation snapshots so both durable views describe the same outcome.
        koe_recording::recover_sessions(&data_root).map_err(|_| McpError::OperationFailed)?;
        let operations = load_operations(&operations_root, &sessions_root)?;
        Ok(Self {
            data_root,
            sessions_root,
            operations_root,
            export_root,
            backend: CpalBackend::default(),
            model_manager,
            operations,
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
        // These tools return detached, pollable operations immediately. A
        // request-scoped progressToken expires with this response and must not
        // be retained for later notifications. Clients poll koe_get_operation
        // or read koe://operations/<id> instead.
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
                        state.error_code = Some(OPERATION_DEADLINE_ERROR.to_owned());
                        OperationState::Failed
                    },
                    TimedOperation::Completed(Err(error)) => {
                        state.error_code = Some(error.code().to_owned());
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
                "model",
                "language",
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
        let model_selector = args.get("model").and_then(Value::as_str).unwrap_or("none");
        let language = args.get("language").and_then(Value::as_str).unwrap_or("en");
        if model_selector.len() > 256 || language.len() > 32 {
            return Err(McpError::InvalidParams);
        }
        let prepared_asr = if model_selector == "none" {
            None
        } else {
            let selector = model_selector
                .parse::<ModelSelector>()
                .map_err(|_| McpError::InvalidParams)?;
            let installed = self
                .model_manager
                .installed_id_for(&selector)
                .map_err(|_| McpError::OperationFailed)?
                .ok_or(McpError::ModelNotFound)?;
            let loaded = block_on(self.model_manager.load(&installed))
                .map_err(|_| McpError::OperationFailed)?;
            let session = block_on(self.model_manager.create_asr_session(
                &installed,
                &AsrSessionSettings {
                    language: Some(language.to_owned()),
                    ..AsrSessionSettings::default()
                },
            ))
            .map_err(|_| McpError::OperationFailed)?;
            Some((
                session,
                TranscriptModel::new(
                    loaded.descriptor.id.0,
                    loaded.descriptor.version.0,
                    loaded.descriptor.variant,
                )
                .map_err(|_| McpError::OperationFailed)?,
            ))
        };
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
        let asr = prepared_asr.map(|(session, model)| {
            McpAsrBridge::spawn(
                session,
                model,
                self.sessions_root
                    .join(session_id.to_string())
                    .join("transcript"),
            )
        });
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
                asr,
                config.sample_rate,
                config.channels,
            );
        });
        self.operations.insert(
            operation_id.to_string(),
            Operation {
                snapshot,
                recording_control: Some(control),
                cancellation,
                task: Some(task),
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
            .ok_or(McpError::OperationNotFound)?;
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
        let snapshot = self
            .operations
            .get(id)
            .ok_or(McpError::OperationNotFound)?
            .snapshot
            .lock()
            .map_err(|_| McpError::OperationFailed)
            .map(|snapshot| snapshot.clone())?;
        persist_operation(&self.operations_root, &snapshot)?;
        Ok(snapshot)
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
                    snapshot.error_code = Some(error_code.to_owned());
                    let _ignored = persist_operation(&self.operations_root, &snapshot);
                }
            }
            let terminal = operation
                .snapshot
                .lock()
                .is_ok_and(|snapshot| !matches!(snapshot.state, OperationState::Running));
            if terminal && let Some(task) = operation.task.take() {
                let _ignored = task.join();
            }
            if let Ok(snapshot) = operation.snapshot.lock() {
                let _ignored = persist_operation(&self.operations_root, &snapshot);
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
            let _ignored = fs::remove_file(operation_path(&self.operations_root, &id));
        }
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
            return Err(McpError::ResourceNotFound);
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
        let canonical = path.canonicalize().map_err(|_| McpError::SessionNotFound)?;
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
        let canonical = path.canonicalize().map_err(|_| McpError::SessionNotFound)?;
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
        let canonical = path.canonicalize().map_err(|_| McpError::SessionNotFound)?;
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
            if let Ok(snapshot) = operation.snapshot.lock() {
                let _ignored = persist_operation(&self.operations_root, &snapshot);
            }
        }
    }
}

fn operation_path(
    root: &Path,
    id: &str,
) -> PathBuf {
    root.join(format!("{id}.json"))
}

fn persist_operation(
    root: &Path,
    snapshot: &OperationSnapshot,
) -> Result<(), McpError> {
    let id = snapshot.operation_id.to_string();
    let destination = operation_path(root, &id);
    let staging = root.join(format!(".{id}.{}.tmp", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec(snapshot).map_err(|_| McpError::OperationFailed)?;
    fs::write(&staging, bytes).map_err(|_| McpError::OperationFailed)?;
    replace_file(&staging, &destination).map_err(|_| {
        let _ignored = fs::remove_file(&staging);
        McpError::OperationFailed
    })
}

/// Replaces a regular file without relying on Unix rename-over-existing
/// semantics. The backup makes the operation work on Windows and permits a
/// best-effort rollback if publishing the staging file fails.
fn replace_file(
    staging: &Path,
    destination: &Path,
) -> io::Result<()> {
    if fs::symlink_metadata(destination).is_err() {
        return fs::rename(staging, destination);
    }
    let backup = destination.with_extension(format!("json.{}.bak", uuid::Uuid::new_v4()));
    fs::rename(destination, &backup)?;
    match fs::rename(staging, destination) {
        Ok(()) => fs::remove_file(backup),
        Err(error) => {
            let _ignored = fs::rename(&backup, destination);
            Err(error)
        },
    }
}

fn load_operations(
    root: &Path,
    sessions_root: &Path,
) -> Result<HashMap<String, Operation>, McpError> {
    let mut operations = HashMap::new();
    for entry in fs::read_dir(root).map_err(|_| McpError::OperationFailed)? {
        let entry = entry.map_err(|_| McpError::OperationFailed)?;
        if !entry
            .file_type()
            .map_err(|_| McpError::OperationFailed)?
            .is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let mut snapshot: OperationSnapshot =
            serde_json::from_slice(&read_limited_file(&entry.path(), MAX_OUTPUT_BYTES)?)
                .map_err(|_| McpError::OperationFailed)?;
        if matches!(snapshot.state, OperationState::Running) {
            snapshot.state = OperationState::Failed;
            snapshot.progress = 100;
            snapshot.error_code = Some(
                snapshot
                    .session_id
                    .and_then(|id| read_manifest(&sessions_root.join(id.to_string())).ok())
                    .map_or("KOE-MCP-OPERATION-INTERRUPTED", |manifest| {
                        if manifest.state == SessionState::RecoveredPartial {
                            "KOE-STORE-RECOVERED-PARTIAL"
                        } else {
                            "KOE-MCP-OPERATION-INTERRUPTED"
                        }
                    })
                    .to_owned(),
            );
            persist_operation(root, &snapshot)?;
        }
        let id = snapshot.operation_id.to_string();
        operations.insert(
            id,
            Operation {
                snapshot: Arc::new(Mutex::new(snapshot)),
                recording_control: None,
                cancellation: tokio_util::sync::CancellationToken::new(),
                task: None,
            },
        );
    }
    Ok(operations)
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

enum McpAsrCommand {
    Chunk { samples: Vec<i16>, start_us: u64 },
    Stop,
}

struct McpAsrBridge {
    sender: std::sync::mpsc::SyncSender<McpAsrCommand>,
    worker: Option<JoinHandle<Result<(), &'static str>>>,
    dropped: usize,
    transcript_dir: PathBuf,
    staging_dir: PathBuf,
}

impl McpAsrBridge {
    fn spawn(
        session: Box<dyn koe_model::StreamingAsrSession>,
        model: TranscriptModel,
        transcript_dir: PathBuf,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(64);
        let staging_dir =
            transcript_dir.with_file_name(format!(".transcript.{}.tmp", uuid::Uuid::new_v4()));
        let worker_staging_dir = staging_dir.clone();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "KOE-MODEL-INTERNAL")?;
            let mut session = session;
            let mut store = TranscriptStore::open(worker_staging_dir)
                .map_err(koe_transcript::TranscriptError::code)?;
            for command in receiver {
                match command {
                    McpAsrCommand::Chunk { samples, start_us } => {
                        runtime
                            .block_on(session.append(koe_model::Pcm16Mono16k {
                                samples,
                                session_start_us: start_us,
                            }))
                            .map_err(koe_model::AsrError::code)?;
                        while let Some(event) = runtime
                            .block_on(session.poll_results())
                            .map_err(koe_model::AsrError::code)?
                        {
                            append_mcp_asr_event(&mut store, &model, &event)?;
                        }
                    },
                    McpAsrCommand::Stop => break,
                }
            }
            let transcript = runtime
                .block_on(session.finish())
                .map_err(koe_model::AsrError::code)?;
            for event in transcript.events {
                append_mcp_asr_event(&mut store, &model, &event)?;
            }
            store
                .finalize()
                .map_err(koe_transcript::TranscriptError::code)?;
            Ok(())
        });
        Self {
            sender,
            worker: Some(worker),
            dropped: 0,
            transcript_dir,
            staging_dir,
        }
    }

    fn feed(
        &mut self,
        samples: Vec<i16>,
        start_us: u64,
    ) -> Result<(), &'static str> {
        match self
            .sender
            .try_send(McpAsrCommand::Chunk { samples, start_us })
        {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped = self.dropped.saturating_add(1);
                Ok(())
            },
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err("KOE-ASR-WORKER-STOPPED"),
        }
    }

    fn finish(self) -> Result<(), &'static str> {
        self.finish_with_timeout(CANCELLATION_GRACE)
    }

    fn finish_with_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<(), &'static str> {
        let deadline = std::time::Instant::now() + timeout;
        let mut stop = McpAsrCommand::Stop;
        let mut stop_failure = None;
        loop {
            match self.sender.try_send(stop) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    stop_failure = Some("KOE-ASR-WORKER-STOPPED");
                    break;
                },
                Err(std::sync::mpsc::TrySendError::Full(command)) => {
                    stop = command;
                    if std::time::Instant::now() >= deadline {
                        stop_failure = Some("KOE-ASR-FINALIZE-TIMEOUT");
                        break;
                    }
                    thread::sleep(Duration::from_millis(2));
                },
            }
        }
        // Closing the channel guarantees a worker blocked on receive can
        // proceed even when a Stop command could not be queued.
        drop(self.sender);
        let worker = self.worker.take().ok_or("KOE-ASR-WORKER-STOPPED")?;
        while !worker.is_finished() {
            if std::time::Instant::now() >= deadline {
                return Err("KOE-ASR-FINALIZE-TIMEOUT");
            }
            thread::sleep(Duration::from_millis(2));
        }
        let worker_result = worker.join().map_err(|_| "KOE-ASR-WORKER-PANICKED")?;
        if self.dropped != 0 {
            let _ignored = fs::remove_dir_all(&self.staging_dir);
            return Err("KOE-ASR-RETRANSCRIPTION-REQUIRED");
        }
        if let Some(error) = stop_failure {
            let _ignored = fs::remove_dir_all(&self.staging_dir);
            return Err(error);
        }
        if let Err(error) = worker_result {
            let _ignored = fs::remove_dir_all(&self.staging_dir);
            return Err(error);
        }
        // Only publish after the worker is joined. A timed-out detached worker
        // can therefore never mutate the canonical transcript after the
        // operation becomes terminal.
        match fs::remove_dir(&self.transcript_dir) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(_) => return Err("KOE-TRANSCRIPT-WRITE-FAILED"),
        }
        fs::rename(&self.staging_dir, &self.transcript_dir)
            .map_err(|_| "KOE-TRANSCRIPT-WRITE-FAILED")
    }
}

fn append_mcp_asr_event(
    store: &mut TranscriptStore,
    model: &TranscriptModel,
    event: &koe_model::AsrEvent,
) -> Result<(), &'static str> {
    if event.text.is_empty() {
        return Ok(());
    }
    let segment = TranscriptSegment::builder(
        event.start_us / 1_000,
        event.end_us / 1_000,
        event.text.clone(),
    )
    .segment_id(SegmentId::from(event.segment_id))
    .state(if event.is_final {
        TranscriptSegmentState::Final
    } else {
        TranscriptSegmentState::Interim
    })
    .model(model.clone())
    .build()
    .map_err(|_| "KOE-TRANSCRIPT-INVALID-SEGMENT")?;
    store
        .append(segment)
        .map_err(koe_transcript::TranscriptError::code)?;
    store
        .checkpoint()
        .map_err(koe_transcript::TranscriptError::code)
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
    mut asr: Option<McpAsrBridge>,
    sample_rate: u32,
    channels: u16,
) {
    let mut terminal = None;
    let mut failure_code = None;
    let mut callback_anchor_ns = None;
    let mut capture_epoch_id = 0_u64;
    let deadline = std::time::Instant::now() + max_duration;
    let mut normalizer = CanonicalNormalizer::new(sample_rate, channels).ok();
    'capture: loop {
        if std::time::Instant::now() >= deadline {
            terminal = Some(false);
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
                if !frame_format_matches(&metadata, sample_rate, channels) {
                    failure_code = Some("KOE-AUDIO-FORMAT-CHANGED");
                    break 'capture;
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
                if let (Some(asr), Some(normalizer)) = (asr.as_mut(), normalizer.as_mut()) {
                    let frames = count / usize::from(metadata.channels.max(1));
                    let capacity = frames.saturating_mul(2).saturating_add(8);
                    let mut canonical = vec![0_i16; capacity];
                    match normalizer.process(&samples[..count], &mut canonical) {
                        Ok(canonical_count) => {
                            for (index, chunk) in
                                canonical[..canonical_count].chunks(2_560).enumerate()
                            {
                                let offset = u64::try_from(index.saturating_mul(2_560))
                                    .unwrap_or(u64::MAX)
                                    .saturating_mul(1_000_000)
                                    / 16_000;
                                if let Err(code) = asr.feed(
                                    chunk.to_vec(),
                                    timeline.session_start_us.saturating_add(offset),
                                ) {
                                    failure_code = Some(code);
                                    break 'capture;
                                }
                            }
                        },
                        Err(error) => {
                            failure_code = Some(error.code());
                            break 'capture;
                        },
                    }
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
    // Every terminal path owns the ASR bridge until its bounded finish/join.
    // This prevents final.json from being written after the operation is
    // already observable as terminal.
    let asr_failure = asr.take().and_then(|asr| asr.finish().err());
    let operation_failure = failure_code.or(stream_failure).or(asr_failure);
    let result = operation_failure.map_or_else(
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
    let terminal_failure = operation_failure.or(shutdown_failure);
    if let Ok(mut state) = snapshot.lock() {
        state.progress = 100;
        if let Some(code) = terminal_failure {
            state.state = OperationState::Failed;
            state.error_code = Some(code.to_owned());
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
                    state.error_code = Some(error.code().to_owned());
                },
            }
        }
    }
}

const fn frame_format_matches(
    metadata: &koe_audio::FrameMetadata,
    sample_rate: u32,
    channels: u16,
) -> bool {
    metadata.sample_rate == sample_rate && metadata.channels == channels
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
    let metadata = fs::symlink_metadata(path).map_err(|_| McpError::ResourceNotFound)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || has_multiple_links(&metadata) {
        return Err(McpError::Unauthorized);
    }
    if metadata.len() > u64::try_from(limit).unwrap_or(u64::MAX) {
        return Err(McpError::ResponseTooLarge);
    }
    let file = fs::File::open(path).map_err(|_| McpError::ResourceNotFound)?;
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
    let value = json!({
        "code": error.koe_code(),
        "message": error.to_string(),
        "remedy": error.remedy(),
        "retryable": error.retryable(),
        "operation_id": Value::Null,
        "diagnostic_id": uuid::Uuid::new_v4().to_string()
    });
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
    let diagnostic_id = uuid::Uuid::new_v4().to_string();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": error.rpc_code(),
            "message": error.to_string(),
            "data": {
                "koe_code": error.koe_code(),
                "remedy": error.remedy(),
                "retryable": error.retryable(),
                "operation_id": Value::Null,
                "diagnostic_id": diagnostic_id
            }
        }
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
            "Start microphone recording with optional installed-model transcription; requires fresh host consent",
            json!({"type":"object","additionalProperties":false,"required":["device_id","consent"],"properties":{"device_id":{"type":"string","maxLength":512},"model":{"type":"string","maxLength":256,"default":"none"},"language":{"type":"string","maxLength":32,"default":"en"},"sample_rate":{"type":"integer","minimum":8000,"maximum":192_000},"channels":{"type":"integer","minimum":1,"maximum":8},"max_duration_seconds":{"type":"integer","minimum":1,"maximum":86_400},"consent":{"const":true}}}),
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
        sync::{Arc, Condvar, Mutex},
        time::Duration,
    };

    use koe_core::{OperationId, SessionId};

    use super::{
        Args, LifecycleState, McpAsrBridge, McpError, Operation, OperationSnapshot, OperationState,
        Server, TimedOperation, error_response, frame_format_matches, persist_operation,
        run_with_deadline, tool_error,
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
    fn detached_operations_are_polled_without_request_scoped_progress() {
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
            },
        );
        {
            let mut state = snapshot.lock().expect("snapshot");
            state.progress = 100;
            state.state = OperationState::Completed;
        }
        assert_eq!(
            server
                .operation_snapshot(&id.to_string())
                .expect("poll snapshot")
                .progress,
            100
        );
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

    #[test]
    fn cancelling_a_running_operation_signals_control_and_token() {
        let root = TempDir::new().expect("temp");
        let mut server = server(&root);
        let id = OperationId::new();
        let (control, receiver) = std::sync::mpsc::channel();
        let cancellation = tokio_util::sync::CancellationToken::new();
        server.operations.insert(
            id.to_string(),
            Operation {
                snapshot: Arc::new(Mutex::new(OperationSnapshot {
                    operation_id: id,
                    session_id: None,
                    state: OperationState::Running,
                    progress: 1,
                    error_code: None,
                })),
                recording_control: Some(control),
                cancellation: cancellation.clone(),
                task: None,
            },
        );
        server
            .stop_recording(
                &serde_json::json!({"operation_id": id, "consent": true}),
                true,
            )
            .expect("cancel");
        assert!(cancellation.is_cancelled());
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_millis(50))
                .expect("control"),
            super::RecordingControl::Cancel
        ));
    }

    #[test]
    fn operations_survive_restart_and_running_work_is_reconciled() {
        let root = TempDir::new().expect("temp");
        let id = OperationId::new();
        {
            let mut server = server(&root);
            server.operations.insert(
                id.to_string(),
                Operation {
                    snapshot: Arc::new(Mutex::new(OperationSnapshot {
                        operation_id: id,
                        session_id: None,
                        state: OperationState::Running,
                        progress: 37,
                        error_code: None,
                    })),
                    recording_control: None,
                    cancellation: tokio_util::sync::CancellationToken::new(),
                    task: None,
                },
            );
            server.operation_snapshot(&id.to_string()).expect("persist");
        }

        let restarted = server(&root);
        let snapshot = restarted
            .operation_snapshot(&id.to_string())
            .expect("reload");
        assert_eq!(snapshot.state, OperationState::Failed);
        assert_eq!(snapshot.progress, 100);
        assert_eq!(
            snapshot.error_code.as_deref(),
            Some("KOE-MCP-OPERATION-INTERRUPTED")
        );
    }

    #[test]
    fn restart_reconciles_the_associated_recording_manifest_and_operation() {
        let root = TempDir::new().expect("temp");
        let mut initial = server(&root);
        let session_id = {
            let mut recorder = koe_recording::SessionRecorder::start(
                koe_recording::RecordingConfig::microphone(root.path(), 16_000, 1),
            )
            .expect("recorder");
            recorder.write_samples(&[1, 2, 3, 4]).expect("audio");
            recorder.checkpoint().expect("checkpoint");
            recorder.session_id()
        };
        let operation_id = OperationId::new();
        initial.operations.insert(
            operation_id.to_string(),
            Operation {
                snapshot: Arc::new(Mutex::new(OperationSnapshot {
                    operation_id,
                    session_id: Some(session_id),
                    state: OperationState::Running,
                    progress: 50,
                    error_code: None,
                })),
                recording_control: None,
                cancellation: tokio_util::sync::CancellationToken::new(),
                task: None,
            },
        );
        initial
            .operation_snapshot(&operation_id.to_string())
            .expect("persist operation");
        drop(initial);

        let restarted = server(&root);
        let snapshot = restarted
            .operation_snapshot(&operation_id.to_string())
            .expect("reconciled operation");
        assert_eq!(snapshot.state, OperationState::Failed);
        assert_eq!(
            snapshot.error_code.as_deref(),
            Some("KOE-STORE-RECOVERED-PARTIAL")
        );
        let manifest: koe_recording::SessionManifest = serde_json::from_slice(
            &std::fs::read(
                root.path()
                    .join("sessions")
                    .join(session_id.to_string())
                    .join("session.json"),
            )
            .expect("manifest"),
        )
        .expect("json");
        assert_eq!(manifest.state, koe_core::SessionState::RecoveredPartial);
    }

    #[test]
    fn generic_operation_failure_is_not_blindly_retryable() {
        assert!(!McpError::OperationFailed.retryable());
        assert!(McpError::Capacity.retryable());
        let envelope = error_response(serde_json::Value::Null, &McpError::OperationFailed);
        assert_eq!(envelope["error"]["data"]["retryable"], false);
        let tool_envelope = tool_error(&McpError::OperationFailed);
        let data = &tool_envelope["structuredContent"];
        assert_eq!(data["retryable"], false);
        assert!(data["remedy"].is_string());
        assert!(data["diagnostic_id"].is_string());
        assert!(data["operation_id"].is_null());
    }

    #[test]
    fn not_found_errors_identify_the_resource_kind() {
        assert_eq!(
            McpError::SessionNotFound.koe_code(),
            "KOE-SESSION-NOT-FOUND"
        );
        assert_eq!(McpError::ModelNotFound.koe_code(), "KOE-MODEL-NOT-FOUND");
        assert_eq!(
            McpError::OperationNotFound.koe_code(),
            "KOE-MCP-OPERATION-NOT-FOUND"
        );
        assert_ne!(
            McpError::ModelNotFound.remedy(),
            McpError::SessionNotFound.remedy()
        );
    }

    #[test]
    fn repeated_operation_persistence_replaces_the_previous_snapshot() {
        let root = TempDir::new().expect("temp");
        let operations = root.path().join("operations");
        std::fs::create_dir(&operations).expect("operations");
        let mut snapshot = OperationSnapshot {
            operation_id: OperationId::new(),
            session_id: None,
            state: OperationState::Running,
            progress: 1,
            error_code: None,
        };
        persist_operation(&operations, &snapshot).expect("initial persist");
        snapshot.state = OperationState::Completed;
        snapshot.progress = 100;
        persist_operation(&operations, &snapshot).expect("replacement persist");
        let bytes = std::fs::read(operations.join(format!("{}.json", snapshot.operation_id)))
            .expect("snapshot");
        let loaded: OperationSnapshot = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(loaded.state, OperationState::Completed);
        assert_eq!(loaded.progress, 100);
    }

    #[test]
    fn shutdown_persists_worker_terminal_state() {
        let root = TempDir::new().expect("temp");
        let id = OperationId::new();
        {
            let mut server = server(&root);
            let snapshot = Arc::new(Mutex::new(OperationSnapshot {
                operation_id: id,
                session_id: None,
                state: OperationState::Running,
                progress: 1,
                error_code: None,
            }));
            let worker_snapshot = Arc::clone(&snapshot);
            let cancellation = tokio_util::sync::CancellationToken::new();
            let worker_cancellation = cancellation.clone();
            let task = std::thread::spawn(move || {
                while !worker_cancellation.is_cancelled() {
                    std::thread::yield_now();
                }
                let mut state = worker_snapshot.lock().expect("snapshot");
                state.state = OperationState::Cancelled;
                state.progress = 100;
            });
            server.operations.insert(
                id.to_string(),
                Operation {
                    snapshot,
                    recording_control: None,
                    cancellation,
                    task: Some(task),
                },
            );
            server.cancel_all();
        }
        let restarted = server(&root);
        let snapshot = restarted
            .operation_snapshot(&id.to_string())
            .expect("reload");
        assert_eq!(snapshot.state, OperationState::Cancelled);
        assert_eq!(snapshot.progress, 100);
    }

    #[test]
    fn protocol_progress_tokens_do_not_create_request_scoped_notifications() {
        let root = TempDir::new().expect("temp");
        let mut server = server(&root);
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"koe_capabilities\",\"arguments\":{},\"_meta\":{\"progressToken\":\"request-token\"}}}\n"
        );
        let mut output = Vec::new();
        server.run(input.as_bytes(), &mut output).expect("run");
        let messages: Vec<serde_json::Value> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("json"))
            .collect();
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|message| {
            message.get("method").and_then(serde_json::Value::as_str)
                != Some("notifications/progress")
        }));
    }

    #[test]
    fn live_asr_bridge_materializes_pcm_results_before_completion() {
        let root = TempDir::new().expect("temp");
        let transcript_dir = root.path().join("transcript");
        let session = koe_model::FixtureAsrSession::new(&koe_model::AsrSessionSettings::default());
        let model = koe_transcript::TranscriptModel::new("fixture", "1", "cpu").expect("model");
        let mut bridge = McpAsrBridge::spawn(Box::new(session), model, transcript_dir.clone());
        bridge
            .feed(vec![1_000; 2_560], 0)
            .expect("feed canonical PCM");
        bridge.finish().expect("finish and join");
        let final_segments: Vec<koe_transcript::TranscriptSegment> = serde_json::from_slice(
            &std::fs::read(transcript_dir.join("final.json")).expect("final transcript"),
        )
        .expect("valid transcript");
        assert!(!final_segments.is_empty());
        assert!(
            final_segments
                .iter()
                .all(koe_transcript::TranscriptSegment::is_final)
        );
    }

    #[test]
    fn runtime_frame_format_must_match_the_negotiated_stream() {
        let metadata = koe_audio::FrameMetadata {
            sample_rate: 48_000,
            channels: 2,
            ..koe_audio::FrameMetadata::default()
        };
        assert!(frame_format_matches(&metadata, 48_000, 2));
        assert!(!frame_format_matches(&metadata, 16_000, 2));
        assert!(!frame_format_matches(&metadata, 48_000, 1));
    }

    #[test]
    fn stalled_asr_finish_is_bounded_and_never_publishes_after_timeout() {
        struct StalledAsr {
            gate: Arc<(Mutex<(bool, bool)>, Condvar)>,
        }
        #[async_trait::async_trait]
        impl koe_model::StreamingAsrSession for StalledAsr {
            async fn append(
                &mut self,
                _chunk: koe_model::Pcm16Mono16k,
            ) -> Result<(), koe_model::AsrError> {
                let (lock, signal) = &*self.gate;
                let mut state = lock.lock().expect("gate");
                state.0 = true;
                signal.notify_all();
                while !state.1 {
                    state = signal.wait(state).expect("wait");
                }
                drop(state);
                Ok(())
            }
            async fn poll_results(
                &mut self
            ) -> Result<Option<koe_model::AsrEvent>, koe_model::AsrError> {
                Ok(None)
            }
            async fn finish(
                self: Box<Self>
            ) -> Result<koe_model::FinalTranscript, koe_model::AsrError> {
                Ok(koe_model::FinalTranscript::default())
            }
        }

        let root = TempDir::new().expect("temp");
        let transcript_dir = root.path().join("transcript");
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let mut bridge = McpAsrBridge::spawn(
            Box::new(StalledAsr {
                gate: Arc::clone(&gate),
            }),
            koe_transcript::TranscriptModel::new("fixture", "1", "cpu").expect("model"),
            transcript_dir.clone(),
        );
        bridge.feed(vec![1], 0).expect("first");
        {
            let (lock, signal) = &*gate;
            let mut state = lock.lock().expect("gate");
            while !state.0 {
                state = signal.wait(state).expect("wait");
            }
            drop(state);
        }
        for index in 0..65 {
            bridge.feed(vec![2], index).expect("bounded feed");
        }
        let started = std::time::Instant::now();
        assert_eq!(
            bridge.finish_with_timeout(Duration::from_millis(20)),
            Err("KOE-ASR-FINALIZE-TIMEOUT")
        );
        assert!(started.elapsed() < Duration::from_millis(200));
        let (lock, signal) = &*gate;
        lock.lock().expect("gate").1 = true;
        signal.notify_all();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!transcript_dir.join("final.json").exists());
    }
}
