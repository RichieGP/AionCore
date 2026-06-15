use std::sync::Arc;

use crate::AgentFactoryDeps;
use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::context::FactoryContext;
use crate::manager::codex_app_server::CodexAppServerAgentManager;
use crate::session_context::CodexAppServerSessionBuildContext;
use crate::types::CodexAppServerResolvedConfig;
use aionui_common::ProviderWithModel;

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    context: CodexAppServerSessionBuildContext,
    model: ProviderWithModel,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let selected_model = context
        .config
        .model
        .clone()
        .or_else(|| context.config.current_model_id.clone())
        .or_else(|| model.use_model.clone())
        .or_else(|| (!model.model.is_empty()).then_some(model.model));
    let selected_sandbox = context
        .config
        .sandbox_mode
        .as_deref()
        .or(context.config.session_mode.as_deref())
        .or(context.config.current_mode_id.as_deref())
        .map(normalize_codex_sandbox_mode)
        .unwrap_or_else(|| "danger-full-access".to_owned());

    let config = CodexAppServerResolvedConfig {
        codex_bin: context.config.codex_bin.clone(),
        codex_home: context.config.codex_home.clone(),
        model: selected_model,
        approval_policy: context
            .config
            .approval_policy
            .clone()
            .unwrap_or_else(|| "never".to_owned()),
        sandbox_mode: selected_sandbox,
        event_log_dir: deps.data_dir.join("codex-app-server-events"),
    };

    let manager = CodexAppServerAgentManager::new(ctx.conversation_id, ctx.workspace, config).await?;
    Ok(AgentInstance::CodexAppServer(Arc::new(manager)))
}

fn normalize_codex_sandbox_mode(mode: &str) -> String {
    match mode {
        "full-access" | "yolo" => "danger-full-access".to_owned(),
        "read-only" => "read-only".to_owned(),
        "workspace-write" => "workspace-write".to_owned(),
        other => other.to_owned(),
    }
}
