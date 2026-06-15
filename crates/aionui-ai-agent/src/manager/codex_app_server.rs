use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::{AgentModeResponse, GetModelInfoResponse, ModelInfoEntry, ModelInfoPayload};
use aionui_common::{AgentKillReason, AgentType, ConversationStatus, ErrorChain, TimestampMs, now_ms};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, Notify, broadcast, oneshot};
use tracing::{error, info, warn};

use crate::AgentRuntime;
use crate::agent_task::IAgentTask;
use crate::error::AgentError;
use crate::protocol::events::{AgentStreamEvent, TextEventData, ThinkingEventData};
use crate::protocol::send_error::AgentSendError;
use crate::types::{CodexAppServerResolvedConfig, SendMessageData};

const INIT_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_TIMEOUT: Duration = Duration::from_secs(600);

pub struct CodexAppServerAgentManager {
    runtime: AgentRuntime,
    client: Arc<CodexAppServerClient>,
    config: Mutex<CodexAppServerResolvedConfig>,
    cancel_notify: Arc<Notify>,
    turn_finished_notify: Arc<Notify>,
}

impl CodexAppServerAgentManager {
    pub async fn new(
        conversation_id: String,
        workspace: String,
        config: CodexAppServerResolvedConfig,
    ) -> Result<Self, AgentError> {
        let runtime = AgentRuntime::new(conversation_id.clone(), workspace.clone(), 128);
        let client = CodexAppServerClient::spawn(&conversation_id, &workspace, &config, runtime.event_sender()).await?;
        runtime.transition_to(ConversationStatus::Pending);
        Ok(Self {
            runtime,
            client: Arc::new(client),
            config: Mutex::new(config),
            cancel_notify: Arc::new(Notify::new()),
            turn_finished_notify: Arc::new(Notify::new()),
        })
    }

    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let client = Arc::clone(&self.client);
        let was_running = self.runtime.status() == Some(ConversationStatus::Running);
        if was_running {
            self.cancel_notify.notify_waiters();
        }
        let turn_finished_notify = Arc::clone(&self.turn_finished_notify);
        let runtime = self.runtime.clone();
        let conversation_id = self.runtime.conversation_id().to_owned();
        Box::pin(async move {
            info!(conversation_id, ?reason, "Codex app-server kill requested");
            client.shutdown().await;
            if was_running {
                let _ = tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime.status() == Some(ConversationStatus::Running) {
                        turn_finished_notify.notified().await;
                    }
                })
                .await;
            }
        })
    }

    pub async fn mode(&self) -> Result<AgentModeResponse, AgentError> {
        let config = self.config.lock().await;
        Ok(AgentModeResponse {
            mode: codex_sandbox_to_aion_mode(&config.sandbox_mode),
            initialized: true,
        })
    }

    pub async fn set_mode(&self, mode: &str) -> Result<(), AgentError> {
        if mode.trim().is_empty() {
            return Err(AgentError::bad_request("mode must not be empty"));
        }
        self.config.lock().await.sandbox_mode = aion_mode_to_codex_sandbox(mode);
        Ok(())
    }

    pub async fn get_model(&self) -> Result<GetModelInfoResponse, AgentError> {
        let config = self.config.lock().await;
        let current = config.model.clone().unwrap_or_else(|| "default".to_owned());
        Ok(GetModelInfoResponse {
            model_info: Some(ModelInfoPayload {
                current_model_id: Some(current.clone()),
                current_model_label: Some(current.clone()),
                available_models: vec![ModelInfoEntry {
                    id: current.clone(),
                    label: current,
                }],
            }),
        })
    }

    pub async fn set_model(&self, model_id: &str) -> Result<(), AgentError> {
        if model_id.trim().is_empty() {
            return Err(AgentError::bad_request("model_id must not be empty"));
        }
        self.config.lock().await.model = Some(model_id.to_owned());
        Ok(())
    }
}

#[async_trait::async_trait]
impl IAgentTask for CodexAppServerAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::CodexAppServer
    }

    fn conversation_id(&self) -> &str {
        self.runtime.conversation_id()
    }

    fn workspace(&self) -> &str {
        self.runtime.workspace()
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.runtime.status()
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_at()
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        let started_at = now_ms();
        self.runtime.bump_activity();
        self.runtime.reset_for_new_turn(ConversationStatus::Running);
        let config = self.config.lock().await.clone();
        let result = tokio::select! {
            result = self.client.run_turn(&data.content, self.runtime.workspace(), &config) => result,
            _ = self.cancel_notify.notified() => Ok(()),
        };
        self.runtime.bump_activity();
        match result {
            Ok(()) => {
                info!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms = now_ms().saturating_sub(started_at),
                    "Codex app-server turn completed"
                );
                self.runtime.emit_finish(None);
                self.turn_finished_notify.notify_waiters();
                Ok(())
            }
            Err(error) => {
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    error = %ErrorChain(&error),
                    "Codex app-server turn failed"
                );
                let send_error = AgentSendError::from_agent_error(error);
                self.runtime.emit_error_data(send_error.stream_error().clone());
                self.turn_finished_notify.notify_waiters();
                Err(send_error)
            }
        }
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        self.cancel_notify.notify_waiters();
        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        let client = Arc::clone(&self.client);
        tokio::spawn(async move {
            client.shutdown().await;
        });
        if self.runtime.status() == Some(ConversationStatus::Running) {
            self.cancel_notify.notify_waiters();
        }
        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            "Codex app-server kill scheduled"
        );
        Ok(())
    }
}

struct CodexAppServerClient {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
    next_id: Mutex<u64>,
    response_waiters: Arc<Mutex<std::collections::HashMap<u64, oneshot::Sender<Result<Value, AgentError>>>>>,
    notification_waiters: Arc<Mutex<Vec<NotificationWaiter>>>,
}

struct NotificationWaiter {
    method: String,
    tx: oneshot::Sender<Value>,
}

impl CodexAppServerClient {
    async fn spawn(
        conversation_id: &str,
        workspace: &str,
        config: &CodexAppServerResolvedConfig,
        event_tx: broadcast::Sender<AgentStreamEvent>,
    ) -> Result<Self, AgentError> {
        std::fs::create_dir_all(&config.event_log_dir)
            .map_err(|e| AgentError::internal(format!("Failed to create Codex app-server event log dir: {e}")))?;
        let event_log = config.event_log_dir.join(format!("{conversation_id}.jsonl"));
        let codex_bin = config.codex_bin.as_deref().unwrap_or("codex");
        let mut cmd = aionui_runtime::Builder::new(codex_bin);
        cmd.arg("-c")
            .arg(format!("approval_policy=\"{}\"", config.approval_policy))
            .arg("-c")
            .arg(format!("sandbox_mode=\"{}\"", config.sandbox_mode));
        if let Some(model) = config.model.as_deref().filter(|value| !value.is_empty()) {
            cmd.arg("-m").arg(model);
        }
        cmd.arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .current_dir(workspace)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env_remove("CODEX_HOME")
            .env_remove("KODO_CODEX_HOME")
            .env_remove("OLLAMA_HOST")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("OPENAI_ORGANIZATION")
            .env_remove("OPENAI_PROJECT");
        if let Some(codex_home) = config.codex_home.as_deref().filter(|value| !value.is_empty()) {
            cmd.env("CODEX_HOME", codex_home);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| AgentError::internal(format!("Failed to spawn Codex app-server from '{codex_bin}': {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::internal("Codex app-server stdin was unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::internal("Codex app-server stdout was unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AgentError::internal("Codex app-server stderr was unavailable"))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let client = Self {
            stdin,
            child: Mutex::new(child),
            next_id: Mutex::new(1),
            response_waiters: Arc::new(Mutex::new(std::collections::HashMap::new())),
            notification_waiters: Arc::new(Mutex::new(Vec::new())),
        };
        client.spawn_stdout_reader(stdout, event_log, event_tx);
        client.spawn_stderr_reader(stderr, conversation_id.to_owned());
        tokio::time::timeout(INIT_TIMEOUT, async {
            let _ = client
                .request(
                    "initialize",
                    json!({
                        "clientInfo": {
                            "name": "aionui-codex-app-server",
                            "title": "Aion Codex App Server Experimental",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "capabilities": { "experimentalApi": true }
                    }),
                )
                .await?;
            client.notify("initialized", json!({})).await
        })
        .await
        .map_err(|_| AgentError::bad_gateway("Timed out initializing Codex app-server"))??;
        Ok(client)
    }

    async fn run_turn(
        &self,
        prompt: &str,
        workspace: &str,
        config: &CodexAppServerResolvedConfig,
    ) -> Result<(), AgentError> {
        if prompt.trim().is_empty() {
            return Err(AgentError::bad_request("Codex app-server prompt must not be empty"));
        }
        let mut thread_params = json!({
            "cwd": workspace,
            "runtimeWorkspaceRoots": [workspace],
            "approvalPolicy": config.approval_policy,
            "sandboxPolicy": config.sandbox_mode,
        });
        if let Some(model) = config.model.as_deref().filter(|value| !value.is_empty()) {
            thread_params["model"] = Value::String(model.to_owned());
        }
        let thread = self.request("thread/start", thread_params).await?;
        let thread_id = thread
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::bad_gateway("Codex app-server did not return a thread id"))?;
        let mut turn_params = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": prompt, "text_elements": [] }],
            "cwd": workspace,
            "runtimeWorkspaceRoots": [workspace],
            "approvalPolicy": config.approval_policy,
            "sandboxPolicy": config.sandbox_mode,
        });
        if let Some(model) = config.model.as_deref().filter(|value| !value.is_empty()) {
            turn_params["model"] = Value::String(model.to_owned());
        }
        let turn = self.request("turn/start", turn_params).await?;
        let turn_id = turn
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::bad_gateway("Codex app-server did not return a turn id"))?;
        let completed = tokio::time::timeout(TURN_TIMEOUT, self.wait_for_notification("turn/completed"))
            .await
            .map_err(|_| AgentError::bad_gateway("Timed out waiting for Codex app-server turn completion"))??;
        let completed_thread_id = completed
            .get("params")
            .and_then(|p| p.get("threadId"))
            .and_then(Value::as_str);
        let completed_turn_id = completed
            .get("params")
            .and_then(|p| p.get("turn"))
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str);
        if completed_thread_id != Some(thread_id) || completed_turn_id != Some(turn_id) {
            return Err(AgentError::bad_gateway("Codex app-server completed an unexpected turn"));
        }
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };
        let (tx, rx) = oneshot::channel();
        self.response_waiters.lock().await.insert(id, tx);
        self.write_json(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        rx.await
            .map_err(|_| AgentError::bad_gateway(format!("Codex app-server response channel closed for {method}")))?
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), AgentError> {
        self.write_json(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn wait_for_notification(&self, method: &str) -> Result<Value, AgentError> {
        let (tx, rx) = oneshot::channel();
        self.notification_waiters.lock().await.push(NotificationWaiter {
            method: method.to_owned(),
            tx,
        });
        rx.await
            .map_err(|_| AgentError::bad_gateway(format!("Codex app-server notification channel closed for {method}")))
    }

    async fn write_json(&self, message: Value) -> Result<(), AgentError> {
        write_json_to_stdin(&self.stdin, message).await
    }

    fn spawn_stdout_reader(
        &self,
        stdout: tokio::process::ChildStdout,
        event_log: PathBuf,
        event_tx: broadcast::Sender<AgentStreamEvent>,
    ) {
        let response_waiters = self.response_waiters.clone();
        let notification_waiters = self.notification_waiters.clone();
        let stdin = Arc::clone(&self.stdin);
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                append_jsonl(&event_log, trimmed);
                let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
                    warn!("Codex app-server emitted invalid JSON");
                    continue;
                };
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    if message.get("method").is_some() {
                        let method = message.get("method").and_then(Value::as_str).unwrap_or("unknown");
                        warn!(method = %method, "Unhandled Codex app-server request");
                        let _ = write_json_to_stdin(
                            &stdin,
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32601,
                                    "message": format!("Aion does not implement Codex app-server client method '{method}'")
                                }
                            }),
                        )
                        .await;
                        continue;
                    }
                    if let Some(tx) = response_waiters.lock().await.remove(&id) {
                        let result = match message.get("error") {
                            Some(error) => Err(AgentError::bad_gateway(format!(
                                "Codex app-server error: {}",
                                error.get("message").and_then(Value::as_str).unwrap_or("unknown error")
                            ))),
                            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(result);
                    }
                    continue;
                }
                if let Some(method) = message.get("method").and_then(Value::as_str) {
                    project_notification(method, &message, &event_tx);
                    let mut waiters = notification_waiters.lock().await;
                    if let Some(index) = waiters.iter().position(|waiter| waiter.method == method) {
                        let waiter = waiters.remove(index);
                        let _ = waiter.tx.send(message);
                    }
                }
            }
        });
    }

    fn spawn_stderr_reader(&self, stderr: tokio::process::ChildStderr, conversation_id: String) {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    warn!(conversation_id, stderr = trimmed, "Codex app-server stderr");
                }
            }
        });
    }

    async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        if let Err(error) = aionui_runtime::kill_process_tree(&mut *child).await {
            warn!(error = %ErrorChain(&error), "Failed to terminate Codex app-server process tree");
        }
    }
}

fn project_notification(method: &str, message: &Value, event_tx: &broadcast::Sender<AgentStreamEvent>) {
    match method {
        "item/agentMessage/delta" => {
            if let Some(delta) = message
                .get("params")
                .and_then(|p| p.get("delta"))
                .and_then(Value::as_str)
            {
                let _ = event_tx.send(AgentStreamEvent::Text(TextEventData {
                    content: delta.to_owned(),
                }));
            }
        }
        "item/started" => {
            let title = message
                .get("params")
                .and_then(|p| p.get("item"))
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("working");
            let _ = event_tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
                content: title.to_owned(),
                subject: Some("Codex".to_owned()),
                duration: None,
                status: Some("in_progress".to_owned()),
            }));
        }
        "turn/completed" => {
            emit_final_text_from_completed_turn(message, event_tx);
        }
        "error" => {
            let message = message
                .get("params")
                .and_then(|p| p.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex app-server emitted an error");
            let _ = event_tx.send(AgentStreamEvent::Error(aionui_api_types::AgentStreamErrorData::legacy(
                message, None,
            )));
        }
        _ => {}
    }
}

fn emit_final_text_from_completed_turn(message: &Value, event_tx: &broadcast::Sender<AgentStreamEvent>) {
    let Some(items) = message
        .get("params")
        .and_then(|p| p.get("turn"))
        .and_then(|turn| turn.get("items"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let final_text = items
        .iter()
        .rev()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("agentMessage")
                && item.get("phase").and_then(Value::as_str) == Some("final_answer")
        })
        .or_else(|| {
            items.iter().rev().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("agentMessage")
                    && item.get("text").and_then(Value::as_str).is_some()
            })
        })
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !final_text.trim().is_empty() {
        let _ = event_tx.send(AgentStreamEvent::Text(TextEventData {
            content: final_text.to_owned(),
        }));
    }
}

async fn write_json_to_stdin(stdin: &Arc<Mutex<ChildStdin>>, message: Value) -> Result<(), AgentError> {
    let raw = serde_json::to_vec(&message)
        .map_err(|e| AgentError::internal(format!("Failed to encode Codex app-server message: {e}")))?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(&raw)
        .await
        .map_err(|e| AgentError::bad_gateway(format!("Failed to write to Codex app-server: {e}")))?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|e| AgentError::bad_gateway(format!("Failed to write to Codex app-server: {e}")))?;
    Ok(())
}

fn append_jsonl(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

fn aion_mode_to_codex_sandbox(mode: &str) -> String {
    match mode {
        "full-access" | "yolo" => "danger-full-access".to_owned(),
        "read-only" => "read-only".to_owned(),
        "workspace-write" => "workspace-write".to_owned(),
        other => other.to_owned(),
    }
}

fn codex_sandbox_to_aion_mode(mode: &str) -> String {
    match mode {
        "danger-full-access" => "full-access".to_owned(),
        other => other.to_owned(),
    }
}
