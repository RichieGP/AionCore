use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::agent_task::AgentInstance;
use crate::capability::cli_process::CliAgentProcess;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use crate::factory::context::FactoryContext;
use crate::manager::acp::{AcpAgentManager, CatalogForwarder};
use crate::session_context::AcpSessionBuildContext;
use agent_client_protocol::schema::{EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio};
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use aionui_common::{CommandSpec, EnvVar};
use aionui_db::IMcpServerRepository;
use aionui_db::models::McpServerRow;
use aionui_mcp::{
    AcpMcpCapabilities, CursorAdapter, McpAgentAdapter, McpProjectionKind, McpServerTransport, QwenAdapter,
    normalize_acp_mcp_capabilities_for_agent_row, parse_acp_mcp_capabilities, plan_mcp_projection,
};
use aionui_runtime::{
    ManagedAcpToolId, ensure_managed_acp_tool_with_reporter, ensure_node_runtime_with_reporter, ensure_runtime_command,
    ensure_runtime_command_with_reporter, resolve_command_path,
};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::runtime_status::{conversation_acp_tool_runtime_reporter, conversation_runtime_reporter};

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: AcpSessionBuildContext,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let belongs_to_team = build_context.team.is_some();
    let mut config = build_context.config;

    // Resolve the catalog row — prefer explicit agent_id, fall
    // back to a vendor-label match for legacy payloads.
    let meta = if let Some(ref agent_id) = config.agent_id {
        deps.agent_registry.get(agent_id).await
    } else if let Some(ref vendor) = config.backend {
        deps.agent_registry.find_builtin_by_backend(vendor).await
    } else {
        None
    }
    .ok_or_else(|| AgentError::bad_request("ACP agent requires either agent_id or backend in extra"))?;

    // Trust the catalog row over the client-supplied `backend` when an
    // `agent_id` was provided. The frontend collapses row-scoped rows
    // (custom ACP / remote) to a shared `custom`/`remote` slot string,
    // which downstream consumers (MCP injection, preset-context
    // composition) would mis-interpret. When the caller only supplied a
    // vendor label (builtin path), we preserve it as-is.
    if config.agent_id.is_some() || config.backend.is_none() {
        config.backend.clone_from(&meta.backend);
    }

    // Inject Guide MCP config for solo (non-team) sessions.
    // Team sessions already carry `team_mcp_stdio_config`; the
    // two are mutually exclusive per the build_new_session_request guard.
    if config.team_mcp_stdio_config.is_some() {
        debug!(ctx.conversation_id, "guide_mcp: skipped: has team_mcp");
    } else if belongs_to_team {
        debug!(
            ctx.conversation_id,
            "guide_mcp: skipped: conversation belongs to a team"
        );
    } else if config.guide_mcp_config.is_some() {
        debug!(
            ctx.conversation_id,
            "guide_mcp: skipped: caller already set guide_mcp_config"
        );
    } else if deps.guide_mcp_config.is_none() {
        debug!(ctx.conversation_id, "guide_mcp: skipped: guide server not running");
    } else {
        config.guide_mcp_config.clone_from(&deps.guide_mcp_config);
        info!(
            ctx.conversation_id,
            guide_mcp_port = deps.guide_mcp_config.as_ref().map(|c| c.port),
            "guide_mcp: injected into solo session"
        );
    }

    let mut command_spec =
        resolve_agent_command_spec(&meta, &ctx.workspace, &ctx.conversation_id, deps.broadcaster.clone()).await?;
    if meta.backend.as_deref() == Some("claude") {
        let cc_switch_env = crate::cc_switch::read_claude_provider_env();
        if !cc_switch_env.is_empty() {
            let keys: Vec<&str> = cc_switch_env.keys().map(|k| k.as_str()).collect();
            for (name, value) in &cc_switch_env {
                command_spec.env.push(aionui_common::EnvVar {
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            tracing::info!(?keys, "cc-switch: env vars injected");
        }
    }
    let session_snapshot = build_context.session_snapshot;

    // Load user-configured MCP servers from the DB so they reach
    // ACP `session/new` mcpServers payload. Without this the agent
    // starts with zero MCP tools even when the user configured them
    // via Settings → MCP (ELECTRON-1JG).
    let mcp_capabilities = meta
        .handshake
        .agent_capabilities
        .as_ref()
        .map(parse_acp_mcp_capabilities)
        .unwrap_or_default();
    let mcp_capabilities = normalize_acp_mcp_capabilities_for_agent_row(
        mcp_capabilities,
        meta.backend.as_deref(),
        meta.command.as_deref(),
        meta.agent_source_info.binary_name.as_deref(),
        &meta.args,
    );

    let user_mcp_rows = match deps.mcp_server_repo.as_ref() {
        Some(repo) => {
            load_selected_user_mcp_rows(repo.as_ref(), config.mcp_server_ids.as_deref(), &ctx.conversation_id).await
        }
        None => Vec::new(),
    };

    sync_native_mcp_config_for_backend(
        &user_mcp_rows,
        &ctx.conversation_id,
        meta.backend.as_deref(),
        &deps.data_dir,
    )
    .await;

    let mcp_awareness_context = build_mcp_awareness_context(
        &user_mcp_rows,
        &config.session_mcp_servers,
        meta.backend.as_deref(),
        &mcp_capabilities,
    );

    let projected_user_mcps = load_user_mcp_servers(
        &user_mcp_rows,
        &ctx.conversation_id,
        meta.backend.as_deref(),
        &mcp_capabilities,
        &deps.data_dir,
    )
    .await;
    let mut session_mcp_servers = projected_user_mcps.servers;
    let mcp_proxy_processes = projected_user_mcps.proxy_processes;
    for server in &config.session_mcp_servers {
        if !session_server_supported_by_capabilities(server, &mcp_capabilities) {
            warn!(
                ctx.conversation_id,
                server_id = %server.id,
                server_name = %server.name,
                "session_mcp: transport unsupported by ACP agent; skipping"
            );
            continue;
        }
        match session_server_to_sdk_mcp_server(server).await {
            Ok(server) => session_mcp_servers.push(server),
            Err(err) => {
                warn!(
                    ctx.conversation_id,
                    server_id = %server.id,
                    server_name = %server.name,
                    error = %err,
                    "session_mcp: failed to convert session snapshot; skipping"
                );
            }
        }
    }

    let params = Arc::new(
        assemble_acp_params(
            ctx.conversation_id.clone(),
            WorkspaceInfo {
                path: ctx.workspace,
                is_custom: ctx.is_custom_workspace,
            },
            meta,
            command_spec,
            config,
            session_mcp_servers,
            mcp_proxy_processes,
            mcp_awareness_context,
            session_snapshot,
            deps.data_dir.clone(),
        )
        .await,
    );

    let skill_mgr = deps.skill_manager.clone();
    let catalog_tx = deps.agent_registry.catalog_sender();

    let (agent, domain_rx, notification_rx) = AcpAgentManager::build(params, skill_mgr, &catalog_tx).await?;

    let arc = Arc::new(agent);
    arc.start_permission_handler();
    arc.start_session_event_tracker(notification_rx);
    CatalogForwarder::spawn(
        arc.agent_id().to_owned(),
        crate::IAgentTask::subscribe(arc.as_ref()),
        catalog_tx,
    );

    // Desired (mode/model/config) are seeded from `params.session_snapshot`
    // inside `AcpAgentManager::new`. The CLI-assigned session id is still
    // loaded here so the first turn after a task rebuild takes the resume
    // path.
    if let Some(sid) = build_context.session_id {
        arc.set_session_id(sid).await;
    }

    // Hand the service the domain event receiver so it can
    // persist user intent changes without reverse-engineering
    // them from CLI observations.
    deps.acp_agent_service.attach(ctx.conversation_id, domain_rx).await;

    // Open the ACP session eagerly so `POST /warmup` returns only after
    // session/new (or claude-meta-resume / session/load) and the first
    // reconcile pass have completed. The persistence consumer must be
    // attached first because warmup can emit the initial SessionAssigned
    // event synchronously while creating a native ACP session.
    arc.warmup_session().await?;

    let instance = AgentInstance::Acp(Arc::clone(&arc));

    Ok(instance)
}

async fn resolve_agent_command_spec(
    meta: &aionui_api_types::AgentMetadata,
    workspace: &str,
    conversation_id: &str,
    broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
) -> Result<CommandSpec, AgentError> {
    if meta.agent_source == aionui_api_types::AgentSource::Builtin
        && let Some(backend) = meta.backend.as_deref()
        && let Some(tool) = ManagedAcpToolId::from_backend(backend)
    {
        return resolve_builtin_managed_acp_command_spec(meta, workspace, conversation_id, broadcaster, tool).await;
    }

    let command = meta
        .command
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::bad_request(format!("Agent '{}' has no spawn command configured", meta.name)))?;
    let reporter = conversation_runtime_reporter(broadcaster, conversation_id.to_owned());
    let resolved = ensure_runtime_command_with_reporter(command, Some(reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let mut args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    args.extend(meta.args.iter().cloned());

    let mut env: Vec<aionui_common::EnvVar> = meta
        .env
        .iter()
        .map(|entry| aionui_common::EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(resolved.env.iter().map(|(name, value)| aionui_common::EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    Ok(CommandSpec {
        command: resolved.program,
        args,
        env,
        cwd: Some(workspace.to_owned()),
    })
}

async fn resolve_builtin_managed_acp_command_spec(
    meta: &aionui_api_types::AgentMetadata,
    workspace: &str,
    conversation_id: &str,
    broadcaster: Arc<dyn aionui_realtime::EventBroadcaster>,
    tool: ManagedAcpToolId,
) -> Result<CommandSpec, AgentError> {
    if let Some(primary) = meta.agent_source_info.binary_name.as_deref()
        && resolve_command_path(primary).is_none()
    {
        return Err(AgentError::bad_request(format!(
            "Agent '{}' requires `{primary}` to be installed and available on PATH",
            meta.name
        )));
    }

    let node_reporter = conversation_runtime_reporter(broadcaster.clone(), conversation_id.to_owned());
    let node_runtime = ensure_node_runtime_with_reporter(Some(node_reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let tool_reporter = conversation_acp_tool_runtime_reporter(broadcaster, conversation_id.to_owned(), tool);
    let managed_tool = ensure_managed_acp_tool_with_reporter(tool, Some(tool_reporter.as_ref()))
        .await
        .map_err(|error| AgentError::bad_request(format!("Agent '{}' CLI unavailable: {error}", meta.name)))?;

    let resolved = managed_tool.command(&node_runtime);

    let args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let mut env: Vec<aionui_common::EnvVar> = meta
        .env
        .iter()
        .map(|entry| aionui_common::EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        })
        .collect();
    env.extend(resolved.env.iter().map(|(name, value)| aionui_common::EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    Ok(CommandSpec {
        command: resolved.program,
        args,
        env,
        cwd: Some(workspace.to_owned()),
    })
}

/// Load the operator's enabled MCP servers from the DB, log+skip any rows
/// whose `transport_config` JSON fails to parse (better to start without one
/// MCP tool than fail the whole session), and return them in SDK shape ready
/// for `NewSessionRequest::mcp_servers`.
///
/// When `selected_ids` is present, those rows define the session snapshot and
/// are injected regardless of the current global `enabled` flag. Legacy
/// conversations without a snapshot still fall back to "all enabled rows".
async fn load_selected_user_mcp_rows(
    repo: &dyn IMcpServerRepository,
    selected_ids: Option<&[String]>,
    conversation_id: &str,
) -> Vec<McpServerRow> {
    let rows_result = match selected_ids {
        Some(ids) => repo.list_by_ids_any(ids).await,
        None => repo.list().await,
    };
    let rows = match rows_result {
        Ok(r) => r,
        Err(err) => {
            warn!(
                conversation_id,
                error = %err,
                "user_mcp: list() failed; skipping injection and native sync"
            );
            return Vec::new();
        }
    };

    rows.into_iter()
        .filter(|row| {
            selected_ids
                .map(|ids| ids.iter().any(|id| id == &row.id))
                .unwrap_or(row.enabled)
        })
        .collect()
}

async fn sync_native_mcp_config_for_backend(
    rows: &[McpServerRow],
    conversation_id: &str,
    backend: Option<&str>,
    data_dir: &Path,
) {
    let adapter: Box<dyn McpAgentAdapter> = match backend {
        Some("cursor") => Box::new(CursorAdapter) as Box<dyn McpAgentAdapter>,
        Some("qwen") => Box::new(QwenAdapter) as Box<dyn McpAgentAdapter>,
        _ => return,
    };

    if rows.is_empty() {
        return;
    }

    match adapter.is_installed().await {
        Ok(true) => {}
        Ok(false) => {
            debug!(
                conversation_id,
                backend = backend.unwrap_or("unknown"),
                "user_mcp: native adapter not installed; skipping native config sync"
            );
            return;
        }
        Err(err) => {
            warn!(
                conversation_id,
                backend = backend.unwrap_or("unknown"),
                error = %err,
                "user_mcp: failed native adapter install check; skipping native config sync"
            );
            return;
        }
    }

    let mut synced = 0usize;
    for row in rows {
        let transport = match McpServerTransport::from_db(&row.transport_type, &row.transport_config) {
            Ok(transport) => transport,
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "user_mcp: failed to parse transport for native config sync; skipping"
                );
                continue;
            }
        };
        let transport = match cursor_native_audit_transport(row, transport, conversation_id, backend, data_dir).await {
            Ok(transport) => transport,
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    error = %err,
                    "user_mcp: failed to wrap Cursor native MCP config for audit; skipping"
                );
                continue;
            }
        };

        match adapter.install_server(&row.name, &transport).await {
            Ok(()) => synced += 1,
            Err(err) => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    backend = backend.unwrap_or("unknown"),
                    error = %err,
                    "user_mcp: failed native MCP config sync"
                );
            }
        }
    }

    if synced > 0 {
        info!(
            conversation_id,
            backend = backend.unwrap_or("unknown"),
            count = synced,
            "user_mcp: synced into native agent MCP config"
        );
    }
}

async fn cursor_native_audit_transport(
    row: &McpServerRow,
    transport: McpServerTransport,
    conversation_id: &str,
    backend: Option<&str>,
    data_dir: &Path,
) -> Result<McpServerTransport, String> {
    let McpServerTransport::Stdio { command, args, env } = transport else {
        return Ok(transport);
    };
    if backend != Some("cursor") {
        return Ok(McpServerTransport::Stdio { command, args, env });
    }

    let wrapper = write_stdio_mcp_audit_wrapper_script(data_dir)?;
    let node = ensure_runtime_command("node")
        .await
        .map_err(|e| format!("failed to resolve node for Cursor MCP audit wrapper: {e}"))?;
    let mut wrapper_args: Vec<String> = node
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();
    wrapper_args.push(wrapper.display().to_string());

    let mut wrapper_env: HashMap<String, String> = node
        .env
        .iter()
        .map(|(name, value)| (name.to_string_lossy().to_string(), value.to_string_lossy().to_string()))
        .collect();
    wrapper_env.insert("AION_MCP_AUDIT_SERVER_NAME".to_owned(), row.name.clone());
    wrapper_env.insert("AION_MCP_AUDIT_COMMAND".to_owned(), command);
    wrapper_env.insert(
        "AION_MCP_AUDIT_ARGS_JSON".to_owned(),
        serde_json::to_string(&args).map_err(|e| format!("failed to encode Cursor audit args: {e}"))?,
    );
    wrapper_env.insert(
        "AION_MCP_AUDIT_ENV_JSON".to_owned(),
        serde_json::to_string(&env).map_err(|e| format!("failed to encode Cursor audit env: {e}"))?,
    );
    wrapper_env.insert(
        "AION_MCP_AUDIT_LOG".to_owned(),
        data_dir
            .join("mcp-audit")
            .join(format!("{conversation_id}.jsonl"))
            .display()
            .to_string(),
    );

    Ok(McpServerTransport::Stdio {
        command: node.program.display().to_string(),
        args: wrapper_args,
        env: wrapper_env,
    })
}

struct ProjectedUserMcps {
    servers: Vec<McpServer>,
    proxy_processes: Vec<Arc<CliAgentProcess>>,
}

struct ProjectedProxyMcp {
    server: McpServer,
    process: Arc<CliAgentProcess>,
}

const STDIO_HTTP_PROXY_SCRIPT: &str = include_str!("../../assets/stdio-to-streamable-http-proxy.mjs");
const STDIO_MCP_AUDIT_WRAPPER_SCRIPT: &str = include_str!("../../assets/stdio-mcp-audit-wrapper.mjs");

async fn start_stdio_http_proxy(
    row: &McpServerRow,
    conversation_id: &str,
    data_dir: &Path,
) -> Result<ProjectedProxyMcp, String> {
    if row.transport_type != "stdio" {
        return Err(format!(
            "proxy only supports stdio source MCPs, got {}",
            row.transport_type
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;
    let command = value
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "stdio: missing command".to_owned())?;
    let args: Vec<String> = value
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mut env_entries: Vec<(String, String)> = value
        .get("env")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    env_entries.sort_by(|a, b| a.0.cmp(&b.0));
    let (resolved_command, args, env) = ensure_stdio_launch(command, &args, &env_entries).await?;
    let port = allocate_loopback_port()?;
    let script_path = write_stdio_http_proxy_script(data_dir)?;
    let node = ensure_runtime_command("node")
        .await
        .map_err(|e| format!("failed to resolve node for MCP proxy: {e}"))?;

    let mut proxy_args: Vec<String> = node
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();
    proxy_args.push(script_path.display().to_string());

    let mut proxy_env: Vec<EnvVar> = node
        .env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.to_string_lossy().to_string(),
            value: value.to_string_lossy().to_string(),
        })
        .collect();
    proxy_env.extend([
        EnvVar {
            name: "AION_MCP_PROXY_NAME".to_owned(),
            value: row.name.clone(),
        },
        EnvVar {
            name: "AION_MCP_PROXY_COMMAND".to_owned(),
            value: resolved_command.display().to_string(),
        },
        EnvVar {
            name: "AION_MCP_PROXY_ARGS_JSON".to_owned(),
            value: serde_json::to_string(&args).map_err(|e| format!("failed to encode proxy args: {e}"))?,
        },
        EnvVar {
            name: "AION_MCP_PROXY_ENV_JSON".to_owned(),
            value: env_to_json(&env)?,
        },
        EnvVar {
            name: "AION_MCP_PROXY_HOST".to_owned(),
            value: "127.0.0.1".to_owned(),
        },
        EnvVar {
            name: "AION_MCP_PROXY_PORT".to_owned(),
            value: port.to_string(),
        },
    ]);

    let command_spec = CommandSpec {
        command: node.program,
        args: proxy_args,
        env: proxy_env,
        cwd: None,
    };
    let process = Arc::new(
        CliAgentProcess::spawn_for_sdk(command_spec, data_dir)
            .await
            .map_err(|e| format!("failed to spawn MCP proxy process: {e}"))?,
    );
    wait_for_proxy_port(port).await.inspect_err(|_err| {
        let process = Arc::clone(&process);
        tokio::spawn(async move {
            let _ = process.kill(Duration::from_millis(250)).await;
        });
    })?;
    let url = format!("http://127.0.0.1:{port}/mcp");
    info!(
        conversation_id,
        server_id = %row.id,
        server_name = %row.name,
        %url,
        "user_mcp: projected stdio MCP through local streamable HTTP proxy"
    );
    Ok(ProjectedProxyMcp {
        server: McpServer::Http(McpServerHttp::new(row.name.clone(), url)),
        process,
    })
}

fn allocate_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("failed to allocate proxy port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to inspect proxy port: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

fn write_stdio_http_proxy_script(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("mcp-proxy");
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create MCP proxy dir {}: {e}", dir.display()))?;
    let path = dir.join("stdio-to-streamable-http-proxy.mjs");
    fs::write(&path, STDIO_HTTP_PROXY_SCRIPT)
        .map_err(|e| format!("failed to write MCP proxy script {}: {e}", path.display()))?;
    Ok(path)
}

fn write_stdio_mcp_audit_wrapper_script(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("mcp-audit");
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create MCP audit dir {}: {e}", dir.display()))?;
    let path = dir.join("stdio-mcp-audit-wrapper.mjs");
    fs::write(&path, STDIO_MCP_AUDIT_WRAPPER_SCRIPT)
        .map_err(|e| format!("failed to write MCP audit wrapper script {}: {e}", path.display()))?;
    Ok(path)
}

fn env_to_json(env: &[EnvVariable]) -> Result<String, String> {
    let mut obj = serde_json::Map::new();
    for item in env {
        obj.insert(item.name.clone(), serde_json::Value::String(item.value.clone()));
    }
    serde_json::to_string(&serde_json::Value::Object(obj)).map_err(|e| format!("failed to encode proxy env: {e}"))
}

async fn wait_for_proxy_port(port: u16) -> Result<(), String> {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..50 {
        if TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("MCP proxy did not listen on {addr} within timeout"))
}

async fn load_user_mcp_servers(
    rows: &[McpServerRow],
    conversation_id: &str,
    backend: Option<&str>,
    capabilities: &AcpMcpCapabilities,
    data_dir: &Path,
) -> ProjectedUserMcps {
    let mut servers = Vec::with_capacity(rows.len());
    let mut proxy_processes = Vec::new();
    for row in rows {
        let projection = plan_mcp_projection(backend, &row.transport_type, capabilities);
        match projection.kind {
            McpProjectionKind::DirectSession => {
                match row_to_sdk_mcp_server(row, backend, conversation_id, data_dir).await {
                    Ok(server) => servers.push(server),
                    Err(err) => {
                        warn!(
                            conversation_id,
                            server_id = %row.id,
                            server_name = %row.name,
                            error = %err,
                            "user_mcp: failed to convert row; skipping"
                        );
                    }
                }
            }
            McpProjectionKind::ProxyRequired => match start_stdio_http_proxy(row, conversation_id, data_dir).await {
                Ok(projected) => {
                    servers.push(projected.server);
                    proxy_processes.push(projected.process);
                }
                Err(err) => {
                    warn!(
                        conversation_id,
                        server_id = %row.id,
                        server_name = %row.name,
                        error = %err,
                        "user_mcp: failed to start stdio-to-http proxy; skipping"
                    );
                }
            },
            McpProjectionKind::NativeConfig | McpProjectionKind::Unsupported => {
                warn!(
                    conversation_id,
                    server_id = %row.id,
                    server_name = %row.name,
                    transport_type = %row.transport_type,
                    projection = ?projection.kind,
                    reason = projection.reason,
                    "user_mcp: not injectable into ACP session/new"
                );
            }
        }
    }

    if !servers.is_empty() {
        info!(
            conversation_id,
            count = servers.len(),
            "user_mcp: injected into session/new"
        );
    }
    if !proxy_processes.is_empty() {
        info!(
            conversation_id,
            count = proxy_processes.len(),
            "user_mcp: stdio-to-http proxy processes started"
        );
    }
    ProjectedUserMcps {
        servers,
        proxy_processes,
    }
}

fn build_mcp_awareness_context(
    rows: &[McpServerRow],
    session_servers: &[SessionMcpServer],
    backend: Option<&str>,
    capabilities: &AcpMcpCapabilities,
) -> Option<String> {
    let mut entries = Vec::new();
    for row in rows {
        let projection = plan_mcp_projection(backend, &row.transport_type, capabilities);
        if !matches!(
            projection.kind,
            McpProjectionKind::DirectSession | McpProjectionKind::NativeConfig | McpProjectionKind::ProxyRequired
        ) {
            continue;
        }
        let tools = cached_tool_names(row.tools.as_deref());
        entries.push(format_mcp_awareness_entry(
            &row.name,
            &row.transport_type,
            projection.kind,
            &tools,
        ));
    }
    for server in session_servers {
        let transport = session_mcp_transport_label(&server.transport);
        let projection = plan_mcp_projection(backend, transport, capabilities);
        if !matches!(projection.kind, McpProjectionKind::DirectSession) {
            continue;
        }
        entries.push(format_mcp_awareness_entry(
            &server.name,
            transport,
            projection.kind,
            &[],
        ));
    }

    if entries.is_empty() {
        return None;
    }

    Some(format!(
        "[MCP Tools]\n\
The following MCP servers are selected for this conversation and should be treated as available when your runtime exposes MCP tools. Prefer these MCP tools over shelling out or searching local config for the same capability.\n\
{}\n\
[/MCP Tools]",
        entries.join("\n")
    ))
}

fn format_mcp_awareness_entry(
    name: &str,
    transport_type: &str,
    projection: McpProjectionKind,
    tools: &[String],
) -> String {
    let delivery = match projection {
        McpProjectionKind::DirectSession => "direct session",
        McpProjectionKind::NativeConfig => "native config",
        McpProjectionKind::ProxyRequired => "proxy required",
        McpProjectionKind::Unsupported => "unsupported",
    };
    if tools.is_empty() {
        format!("- {name}: transport={transport_type}, delivery={delivery}")
    } else {
        format!(
            "- {name}: transport={transport_type}, delivery={delivery}, cached_tools={}",
            tools.join(", ")
        )
    }
}

fn cached_tool_names(raw_tools: Option<&str>) -> Vec<String> {
    let Some(raw) = raw_tools else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.get("name").and_then(|name| name.as_str()).map(str::to_owned))
        .collect()
}

fn session_mcp_transport_label(transport: &SessionMcpTransport) -> &'static str {
    match transport {
        SessionMcpTransport::Stdio { .. } => "stdio",
        SessionMcpTransport::Http { .. } => "http",
        SessionMcpTransport::Sse { .. } => "sse",
        SessionMcpTransport::StreamableHttp { .. } => "streamable_http",
    }
}

/// Convert an `McpServerRow` into the SDK `McpServer` shape used by
/// `NewSessionRequest::mcp_servers`. Returns an error string when
/// `transport_config` is malformed or required fields are missing.
async fn row_to_sdk_mcp_server(
    row: &McpServerRow,
    backend: Option<&str>,
    conversation_id: &str,
    data_dir: &Path,
) -> Result<McpServer, String> {
    let value: serde_json::Value =
        serde_json::from_str(&row.transport_config).map_err(|e| format!("invalid transport_config JSON: {e}"))?;

    match row.transport_type.as_str() {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "stdio: missing command".to_owned())?;
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut env_entries: Vec<(String, String)> = value
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            env_entries.sort_by(|a, b| a.0.cmp(&b.0));
            let (mut resolved_command, mut args, mut env) = ensure_stdio_launch(command, &args, &env_entries).await?;

            if backend == Some("cursor") {
                let wrapper = write_stdio_mcp_audit_wrapper_script(data_dir)?;
                let node = ensure_runtime_command("node")
                    .await
                    .map_err(|e| format!("failed to resolve node for MCP audit wrapper: {e}"))?;
                let original_command = resolved_command.display().to_string();
                let original_args = args;
                let original_env = env.clone();
                resolved_command = node.program;
                args = node
                    .args_prefix
                    .iter()
                    .map(|arg| arg.to_string_lossy().to_string())
                    .collect();
                args.push(wrapper.display().to_string());
                env.extend(node.env.iter().map(|(name, value)| {
                    EnvVariable::new(name.to_string_lossy().to_string(), value.to_string_lossy().to_string())
                }));
                env.push(EnvVariable::new("AION_MCP_AUDIT_SERVER_NAME", row.name.clone()));
                env.push(EnvVariable::new("AION_MCP_AUDIT_COMMAND", original_command));
                env.push(EnvVariable::new(
                    "AION_MCP_AUDIT_ARGS_JSON",
                    serde_json::to_string(&original_args).map_err(|e| format!("failed to encode audit args: {e}"))?,
                ));
                env.push(EnvVariable::new("AION_MCP_AUDIT_ENV_JSON", env_to_json(&original_env)?));
                env.push(EnvVariable::new(
                    "AION_MCP_AUDIT_LOG",
                    data_dir
                        .join("mcp-audit")
                        .join(format!("{conversation_id}.jsonl"))
                        .display()
                        .to_string(),
                ));
            }

            let stdio = McpServerStdio::new(row.name.clone(), resolved_command)
                .args(args)
                .env(env);
            Ok(McpServer::Stdio(stdio))
        }
        "http" | "streamable_http" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "http: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Http(
                McpServerHttp::new(row.name.clone(), url).headers(headers),
            ))
        }
        "sse" => {
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "sse: missing url".to_owned())?;
            let headers = parse_headers(value.get("headers"));
            Ok(McpServer::Sse(
                McpServerSse::new(row.name.clone(), url).headers(headers),
            ))
        }
        other => Err(format!("unknown transport type: {other}")),
    }
}

fn parse_headers(value: Option<&serde_json::Value>) -> Vec<HttpHeader> {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> = obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect()
}

async fn session_server_to_sdk_mcp_server(server: &SessionMcpServer) -> Result<McpServer, String> {
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            if command.is_empty() {
                return Err("stdio: missing command".to_owned());
            }
            let mut entries: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let (command, args, env) = ensure_stdio_launch(command, args, &entries).await?;
            Ok(McpServer::Stdio(
                McpServerStdio::new(server.name.clone(), command).args(args).env(env),
            ))
        }
        SessionMcpTransport::Http { url, headers } | SessionMcpTransport::StreamableHttp { url, headers } => {
            if url.is_empty() {
                return Err("http: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect();
            Ok(McpServer::Http(
                McpServerHttp::new(server.name.clone(), url).headers(headers),
            ))
        }
        SessionMcpTransport::Sse { url, headers } => {
            if url.is_empty() {
                return Err("sse: missing url".to_owned());
            }
            let mut entries: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let headers = entries.into_iter().map(|(k, v)| HttpHeader::new(k, v)).collect();
            Ok(McpServer::Sse(
                McpServerSse::new(server.name.clone(), url).headers(headers),
            ))
        }
    }
}

async fn ensure_stdio_launch(
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<(std::path::PathBuf, Vec<String>, Vec<EnvVariable>), String> {
    let resolved = ensure_runtime_command(command)
        .await
        .map_err(|error| error.to_string())?;

    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: Vec<EnvVariable> = env
        .iter()
        .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
        .collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| {
        EnvVariable::new(
            name.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }));

    Ok((resolved.program, final_args, final_env))
}

fn session_server_supported_by_capabilities(server: &SessionMcpServer, capabilities: &AcpMcpCapabilities) -> bool {
    match server.transport {
        SessionMcpTransport::Stdio { .. } => capabilities.stdio,
        SessionMcpTransport::Http { .. } | SessionMcpTransport::StreamableHttp { .. } => capabilities.http,
        SessionMcpTransport::Sse { .. } => capabilities.sse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;
    use aionui_runtime::init as init_runtime;
    use std::sync::OnceLock;
    use std::{mem, path::PathBuf};

    fn make_row(
        name: &str,
        transport_type: &str,
        transport_config: &str,
        enabled: bool,
        builtin: bool,
    ) -> McpServerRow {
        McpServerRow {
            id: format!("mcp_{name}"),
            name: name.to_owned(),
            description: None,
            enabled,
            transport_type: transport_type.into(),
            transport_config: transport_config.into(),
            tools: None,
            last_test_status: "disconnected".into(),
            last_connected: None,
            original_json: None,
            builtin,
            deleted_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn stdio_config_for_existing_command() -> String {
        let command = std::env::current_exe()
            .expect("current test executable")
            .to_string_lossy()
            .into_owned();
        serde_json::json!({
            "command": command,
            "args": [],
            "env": {},
        })
        .to_string()
    }

    fn make_agent_meta(
        backend: Option<&str>,
        command: Option<&str>,
        args: Vec<&str>,
    ) -> aionui_api_types::AgentMetadata {
        aionui_api_types::AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: backend.map(str::to_owned),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Custom,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: command.map(str::to_owned),
            resolved_command: None,
            args: args.into_iter().map(str::to_owned).collect(),
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: false,
            handshake: aionui_api_types::AgentHandshake::default(),
        }
    }

    #[test]
    fn normalize_mcp_capabilities_enables_stdio_for_kodo_acp_custom_agent() {
        let caps = AcpMcpCapabilities {
            stdio: false,
            http: true,
            sse: false,
        };
        let meta = make_agent_meta(
            None,
            Some("/Users/richard/.local/bin/kodo"),
            vec!["acp", "--backend", "codex-ollama"],
        );

        let normalized = normalize_acp_mcp_capabilities_for_agent_row(
            caps,
            meta.backend.as_deref(),
            meta.command.as_deref(),
            meta.agent_source_info.binary_name.as_deref(),
            &meta.args,
        );

        assert!(normalized.stdio);
        assert!(normalized.http);
        assert!(!normalized.sse);
    }

    #[test]
    fn normalize_mcp_capabilities_preserves_unknown_custom_agent() {
        let caps = AcpMcpCapabilities {
            stdio: false,
            http: true,
            sse: false,
        };
        let meta = make_agent_meta(None, Some("/usr/local/bin/unknown-acp"), vec!["acp"]);

        let normalized = normalize_acp_mcp_capabilities_for_agent_row(
            caps.clone(),
            meta.backend.as_deref(),
            meta.command.as_deref(),
            meta.agent_source_info.binary_name.as_deref(),
            &meta.args,
        );

        assert_eq!(normalized, caps);
    }

    fn path_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[cfg(unix)]
    fn test_runtime_data_dir() -> &'static PathBuf {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().to_path_buf();
            mem::forget(temp);
            init_runtime(&path);
            path
        })
    }

    #[cfg(unix)]
    fn install_fake_bundled_runtime() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let runtime_root = tmp.path().join("node").join("node-v24.11.0-darwin-arm64");
        let bin = runtime_root.join("bin");
        std::fs::create_dir_all(&bin).expect("create bin");

        for tool in ["node", "npm", "npx"] {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\necho v24.11.0\n").expect("write tool");
            let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }

        tmp
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn row_to_sdk_stdio_flattens_resolved_npx_command() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        unsafe { std::env::set_var("AIONUI_BUNDLED_MANAGED_RESOURCES", runtime.path()) };

        let row = make_row(
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );

        let server = row_to_sdk_mcp_server(&row, Some("claude"), "conv-1", test_runtime_data_dir())
            .await
            .expect("convert");
        unsafe { std::env::remove_var("AIONUI_BUNDLED_MANAGED_RESOURCES") };
        match server {
            McpServer::Stdio(s) => {
                let command = s.command.to_string_lossy();
                assert_ne!(command, "npx");
                assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
                assert_eq!(s.args, vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]);
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_agent_command_spec_flattens_bare_npx_command() {
        let _lock = path_test_lock().lock().await;
        let runtime = install_fake_bundled_runtime();
        let _runtime_data_dir = test_runtime_data_dir();
        unsafe { std::env::set_var("AIONUI_BUNDLED_MANAGED_RESOURCES", runtime.path()) };

        let meta = aionui_api_types::AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("custom".into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Custom,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: Some("npx".into()),
            resolved_command: None,
            args: vec!["-y".into(), "@scope/test-agent".into()],
            env: vec![aionui_api_types::AgentEnvEntry {
                name: "K".into(),
                value: "V".into(),
                description: None,
            }],
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: false,
            handshake: aionui_api_types::AgentHandshake::default(),
        };

        let spec = resolve_agent_command_spec(
            &meta,
            "/tmp/workspace",
            "conv-acp",
            Arc::new(BroadcastEventBus::new(16)),
        )
        .await
        .expect("resolved command spec");

        unsafe { std::env::remove_var("AIONUI_BUNDLED_MANAGED_RESOURCES") };
        let command = spec.command.to_string_lossy();
        assert_ne!(command, "npx");
        assert!(command.ends_with("/npx"), "unexpected stdio command path: {command}");
        assert_eq!(spec.args, vec!["-y".to_owned(), "@scope/test-agent".to_owned()]);
        assert!(spec.env.iter().any(|entry| entry.name == "K" && entry.value == "V"));
        assert_eq!(spec.cwd.as_deref(), Some("/tmp/workspace"));
    }

    #[tokio::test]
    async fn row_to_sdk_stdio_roundtrip() {
        let row = make_row(
            "ctx7",
            "stdio",
            r#"{"command":"npx","args":["-y","@upstash/context7-mcp"],"env":{"K":"V"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row, Some("claude"), "conv-1", test_runtime_data_dir())
            .await
            .expect("convert");
        match server {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "ctx7");
                let command = s.command.to_string_lossy();
                assert!(
                    command == "npx" || command.ends_with("/npx"),
                    "unexpected stdio command path: {command}",
                );
                assert_eq!(s.args, vec!["-y".to_owned(), "@upstash/context7-mcp".to_owned()]);
                assert!(
                    s.env.iter().any(|entry| entry.name == "K" && entry.value == "V"),
                    "missing user-provided env in stdio launch"
                );
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[tokio::test]
    async fn row_to_sdk_http_with_headers() {
        let row = make_row(
            "remote",
            "http",
            r#"{"url":"https://example.com/mcp","headers":{"Authorization":"Bearer tok"}}"#,
            true,
            false,
        );
        let server = row_to_sdk_mcp_server(&row, Some("claude"), "conv-1", test_runtime_data_dir())
            .await
            .expect("convert");
        match server {
            McpServer::Http(h) => {
                assert_eq!(h.name, "remote");
                assert_eq!(h.url, "https://example.com/mcp");
                assert_eq!(h.headers.len(), 1);
                assert_eq!(h.headers[0].name, "Authorization");
                assert_eq!(h.headers[0].value, "Bearer tok");
            }
            _ => panic!("expected Http"),
        }
    }

    #[tokio::test]
    async fn row_to_sdk_unknown_transport_type_errors() {
        let row = make_row("bad", "websocket", "{}", true, false);
        assert!(
            row_to_sdk_mcp_server(&row, Some("claude"), "conv-1", test_runtime_data_dir())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn row_to_sdk_invalid_json_errors() {
        let row = make_row("bad", "stdio", "not-json", true, false);
        assert!(
            row_to_sdk_mcp_server(&row, Some("claude"), "conv-1", test_runtime_data_dir())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn row_to_sdk_stdio_missing_command_errors() {
        let row = make_row("bad", "stdio", r#"{"args":[]}"#, true, false);
        assert!(
            row_to_sdk_mcp_server(&row, Some("claude"), "conv-1", test_runtime_data_dir())
                .await
                .is_err()
        );
    }

    #[test]
    fn mcp_awareness_context_lists_all_accessible_selected_mcps() {
        let mut searcher = make_row(
            "kodo-searcher",
            "stdio",
            r#"{"command":"node","args":["searcher.mjs"],"env":{}}"#,
            true,
            false,
        );
        searcher.tools = Some(
            serde_json::json!([
                { "name": "coding_threads_search" },
                { "name": "coding_thread_handoff" }
            ])
            .to_string(),
        );
        let mut ghidra = make_row(
            "ghidra-mcp",
            "stdio",
            r#"{"command":"node","args":["ghidra.mjs"],"env":{}}"#,
            true,
            false,
        );
        ghidra.tools = Some(serde_json::json!([{ "name": "ghidra_status" }]).to_string());

        let ctx = build_mcp_awareness_context(
            &[searcher, ghidra],
            &[],
            Some("cursor"),
            &AcpMcpCapabilities {
                stdio: true,
                http: true,
                sse: true,
            },
        )
        .expect("awareness context");

        assert!(ctx.contains("[MCP Tools]"));
        assert!(ctx.contains("kodo-searcher"));
        assert!(ctx.contains("coding_threads_search"));
        assert!(ctx.contains("ghidra-mcp"));
        assert!(ctx.contains("ghidra_status"));
    }

    #[test]
    fn mcp_awareness_context_skips_unsupported_mcps() {
        let row = make_row(
            "local-only",
            "stdio",
            r#"{"command":"node","args":["server.mjs"],"env":{}}"#,
            true,
            false,
        );
        let ctx = build_mcp_awareness_context(
            &[row],
            &[],
            Some("unknown"),
            &AcpMcpCapabilities {
                stdio: false,
                http: false,
                sse: false,
            },
        );

        assert!(ctx.is_none());
    }

    #[test]
    fn mcp_awareness_context_lists_proxy_projected_mcps() {
        let row = make_row(
            "stdio-through-proxy",
            "stdio",
            r#"{"command":"node","args":["server.mjs"],"env":{}}"#,
            true,
            false,
        );
        let ctx = build_mcp_awareness_context(
            &[row],
            &[],
            Some("network-only"),
            &AcpMcpCapabilities {
                stdio: false,
                http: true,
                sse: false,
            },
        )
        .expect("proxy awareness context");

        assert!(ctx.contains("stdio-through-proxy"));
        assert!(ctx.contains("delivery=proxy required"));
    }

    // -- load_user_mcp_servers integration -----------------------------------

    use async_trait::async_trait;
    use std::sync::Arc;

    struct MockRepo {
        rows: Vec<McpServerRow>,
        fail: bool,
    }

    #[async_trait]
    impl IMcpServerRepository for MockRepo {
        async fn list(&self) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                Err(aionui_db::DbError::Init("simulated".into()))
            } else {
                Ok(self.rows.clone())
            }
        }
        async fn find_by_id(&self, _id: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn find_by_name(&self, _name: &str) -> Result<Option<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn list_by_ids_any(&self, ids: &[String]) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            if self.fail {
                return Err(aionui_db::DbError::Init("simulated".into()));
            }
            Ok(ids
                .iter()
                .filter_map(|id| self.rows.iter().find(|row| row.id == *id).cloned())
                .collect())
        }
        async fn create(
            &self,
            _params: aionui_db::CreateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _id: &str,
            _params: aionui_db::UpdateMcpServerParams<'_>,
        ) -> Result<McpServerRow, aionui_db::DbError> {
            unimplemented!()
        }
        async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
        async fn batch_upsert(
            &self,
            _servers: &[aionui_db::CreateMcpServerParams<'_>],
        ) -> Result<Vec<McpServerRow>, aionui_db::DbError> {
            unimplemented!()
        }
        async fn update_status(
            &self,
            _id: &str,
            _status: &str,
            _last_connected: Option<aionui_common::TimestampMs>,
        ) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
        async fn update_tools(&self, _id: &str, _tools: Option<&str>) -> Result<(), aionui_db::DbError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_disabled_and_keeps_enabled_builtin() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("user-enabled", "stdio", &stdio_config, true, false),
                make_row("user-disabled", "stdio", &stdio_config, false, false),
                make_row("builtin", "stdio", &stdio_config, true, true),
            ],
            fail: false,
        });
        let projected = load_user_mcp_servers_from_repo(repo.as_ref(), None, "conv-1", Some("claude"), &caps).await;
        let servers = projected.servers;
        assert_eq!(servers.len(), 2);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "user-enabled"),
            _ => panic!("expected stdio"),
        }
        match &servers[1] {
            McpServer::Stdio(s) => assert_eq!(s.name, "builtin"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_returns_empty_on_repo_failure() {
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![],
            fail: true,
        });
        let projected = load_user_mcp_servers_from_repo(repo.as_ref(), None, "conv-1", Some("claude"), &caps).await;
        let servers = projected.servers;
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_malformed_rows_but_keeps_others() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("good", "stdio", &stdio_config, true, false),
                make_row("bad", "stdio", "not-json", true, false),
            ],
            fail: false,
        });
        let projected = load_user_mcp_servers_from_repo(repo.as_ref(), None, "conv-1", Some("claude"), &caps).await;
        let servers = projected.servers;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "good"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_wraps_cursor_stdio_with_audit_logger() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![make_row("kodo-searcher", "stdio", &stdio_config, true, false)],
            fail: false,
        });
        let projected =
            load_user_mcp_servers_from_repo(repo.as_ref(), None, "conv-cursor", Some("cursor"), &caps).await;
        let servers = projected.servers;
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, "kodo-searcher");
                assert!(
                    s.args.iter().any(|arg| arg.ends_with("stdio-mcp-audit-wrapper.mjs")),
                    "Cursor MCP stdio server should launch through the audit wrapper"
                );
                let env: std::collections::HashMap<_, _> = s
                    .env
                    .iter()
                    .map(|entry| (entry.name.as_str(), entry.value.as_str()))
                    .collect();
                assert_eq!(env.get("AION_MCP_AUDIT_SERVER_NAME"), Some(&"kodo-searcher"));
                assert!(
                    env.get("AION_MCP_AUDIT_LOG")
                        .is_some_and(|path| path.ends_with("conv-cursor.jsonl"))
                );
            }
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn cursor_native_audit_transport_wraps_stdio_config() {
        let row = make_row(
            "kodo-searcher",
            "stdio",
            &stdio_config_for_existing_command(),
            true,
            false,
        );
        let transport = McpServerTransport::from_db(&row.transport_type, &row.transport_config).unwrap();
        let wrapped = cursor_native_audit_transport(
            &row,
            transport,
            "conv-native-cursor",
            Some("cursor"),
            test_runtime_data_dir(),
        )
        .await
        .expect("wrap");

        match wrapped {
            McpServerTransport::Stdio { command, args, env } => {
                assert!(command.ends_with("node") || command.contains("node"));
                assert!(
                    args.iter().any(|arg| arg.ends_with("stdio-mcp-audit-wrapper.mjs")),
                    "native Cursor MCP config should launch through the audit wrapper"
                );
                assert_eq!(
                    env.get("AION_MCP_AUDIT_SERVER_NAME").map(String::as_str),
                    Some("kodo-searcher")
                );
                assert!(
                    env.get("AION_MCP_AUDIT_LOG")
                        .is_some_and(|path| path.ends_with("conv-native-cursor.jsonl"))
                );
            }
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_uses_selected_snapshot_over_enabled_state() {
        let stdio_config = stdio_config_for_existing_command();
        let caps = AcpMcpCapabilities {
            stdio: true,
            http: true,
            sse: true,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![
                make_row("enabled", "stdio", &stdio_config, true, false),
                make_row("disabled-picked", "stdio", &stdio_config, false, false),
            ],
            fail: false,
        });

        let selected = vec!["mcp_disabled-picked".to_owned()];
        let projected =
            load_user_mcp_servers_from_repo(repo.as_ref(), Some(&selected), "conv-1", Some("claude"), &caps).await;
        let servers = projected.servers;

        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "disabled-picked"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn load_user_mcp_servers_skips_rows_unsupported_by_capabilities() {
        let caps = AcpMcpCapabilities {
            stdio: false,
            http: false,
            sse: false,
        };
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![make_row(
                "stdio-only",
                "stdio",
                r#"{"command":"npx","args":[],"env":{}}"#,
                true,
                false,
            )],
            fail: false,
        });

        let projected = load_user_mcp_servers_from_repo(repo.as_ref(), None, "conv-1", Some("unknown"), &caps).await;
        let servers = projected.servers;
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn load_user_mcp_servers_keeps_stdio_for_normalized_cursor_backend() {
        let caps = aionui_mcp::normalize_acp_mcp_capabilities_for_backend(
            AcpMcpCapabilities {
                stdio: false,
                http: true,
                sse: true,
            },
            Some("cursor"),
        );
        let repo: Arc<dyn IMcpServerRepository> = Arc::new(MockRepo {
            rows: vec![make_row(
                "cursor-stdio",
                "stdio",
                r#"{"command":"npx","args":[],"env":{}}"#,
                true,
                false,
            )],
            fail: false,
        });

        let projected = load_user_mcp_servers_from_repo(repo.as_ref(), None, "conv-1", Some("cursor"), &caps).await;
        let servers = projected.servers;
        assert_eq!(servers.len(), 1);
    }

    async fn load_user_mcp_servers_from_repo(
        repo: &dyn IMcpServerRepository,
        selected_ids: Option<&[String]>,
        conversation_id: &str,
        backend: Option<&str>,
        capabilities: &AcpMcpCapabilities,
    ) -> ProjectedUserMcps {
        let rows = load_selected_user_mcp_rows(repo, selected_ids, conversation_id).await;
        load_user_mcp_servers(&rows, conversation_id, backend, capabilities, &std::env::temp_dir()).await
    }
}
