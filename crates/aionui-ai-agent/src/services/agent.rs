//! Business-logic layer for the ai-agent crate.
//!
//! Per `AGENTS.md` "Domain Crate Structure", this is the sole location
//! for agent-related business logic. HTTP handlers in `routes/` should
//! only extract inputs, call methods on this service, and wrap the
//! result in `ApiResponse`.
//!
//! Session-scoped operations (mode/model/config/usage/capabilities/
//! slash-commands/side-question/workspace/openclaw-runtime) now live in
//! `aionui-conversation::ConversationService`, which dispatches through
//! `AgentInstance`. This service retains only agent-catalog and
//! ACP health-check responsibilities, plus support for the custom-agent
//! CRUD endpoints (see `services::custom`).

use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{
    AcpHealthCheckRequest, AcpHealthCheckResponse, AgentMetadata, ProviderHealthCheckRequest,
    ProviderHealthCheckResponse,
};
use aionui_db::{IProviderRepository, UpsertAgentMetadataParams};
use aionui_extension::kodo_discovery::discover_kodo_acp_adapters;
use aionui_realtime::EventBroadcaster;
use tracing::warn;

use super::provider_health::ProviderHealthCheckService;
use crate::error::AgentError;
use crate::registry::AgentRegistry;

pub struct AgentService {
    registry: Arc<AgentRegistry>,
    broadcaster: Arc<dyn EventBroadcaster>,
    data_dir: PathBuf,
    provider_health: ProviderHealthCheckService,
}

impl AgentService {
    pub fn new(
        registry: Arc<AgentRegistry>,
        broadcaster: Arc<dyn EventBroadcaster>,
        provider_repo: Arc<dyn IProviderRepository>,
        encryption_key: [u8; 32],
        data_dir: PathBuf,
    ) -> Arc<Self> {
        let provider_health = ProviderHealthCheckService::new(provider_repo, encryption_key, data_dir.clone());
        Arc::new(Self {
            registry,
            broadcaster,
            data_dir,
            provider_health,
        })
    }

    /// Data directory used by the custom-agent probe to spawn CLI
    /// processes with a stable cwd.
    pub(crate) fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Registry accessor consumed by the `services::custom` submodule
    /// for direct repository access (upsert / delete / enable toggle).
    pub(crate) fn registry(&self) -> &Arc<AgentRegistry> {
        &self.registry
    }

    pub(crate) fn broadcaster(&self) -> &Arc<dyn EventBroadcaster> {
        &self.broadcaster
    }
}

// Agent operations
impl AgentService {
    pub async fn list_agents(&self) -> Result<Vec<AgentMetadata>, AgentError> {
        self.sync_kodo_adapters().await;
        Ok(self
            .registry
            .list_all()
            .await
            .into_iter()
            .filter(|agent| agent.agent_type.supports_new_conversation())
            .collect())
    }

    pub async fn refresh_agents(&self) -> Result<Vec<AgentMetadata>, AgentError> {
        self.sync_kodo_adapters().await;
        self.registry.refresh_availability().await;
        Ok(self
            .registry
            .list_all()
            .await
            .into_iter()
            .filter(|agent| agent.agent_type.supports_new_conversation())
            .collect())
    }

    pub async fn acp_health_check(&self, req: AcpHealthCheckRequest) -> Result<AcpHealthCheckResponse, AgentError> {
        Ok(crate::protocol::cli_detect::health_check(&self.registry, &req.backend).await)
    }

    pub async fn provider_health_check(
        &self,
        req: ProviderHealthCheckRequest,
    ) -> Result<ProviderHealthCheckResponse, AgentError> {
        self.provider_health.health_check(req).await
    }

    async fn sync_kodo_adapters(&self) {
        if let Err(error) = self.sync_kodo_adapters_inner().await {
            warn!(error = %error, "Kodo adapter agent sync failed");
        }
    }

    async fn sync_kodo_adapters_inner(&self) -> Result<(), AgentError> {
        let adapters = discover_kodo_acp_adapters().await;
        if adapters.is_empty() {
            return Ok(());
        }

        let mut changed = false;
        for adapter in adapters {
            let command = adapter
                .default_cli_path
                .as_deref()
                .or(adapter.cli_command.as_deref())
                .unwrap_or("kodo");
            let args_json = serde_json::to_string(&adapter.acp_args)
                .map_err(|e| AgentError::internal(format!("encode args: {e}")))?;
            let env_entries: Vec<aionui_api_types::AgentEnvEntry> = adapter
                .env
                .iter()
                .map(|(name, value)| aionui_api_types::AgentEnvEntry {
                    name: name.clone(),
                    value: value.clone(),
                    description: None,
                })
                .collect();
            let env_json =
                serde_json::to_string(&env_entries).map_err(|e| AgentError::internal(format!("encode env: {e}")))?;
            let source_info_json = serde_json::json!({
                "binary_name": command,
                "hub_package_id": adapter.extension_name,
            })
            .to_string();
            let available_models_json = if adapter.models.is_empty() {
                None
            } else {
                let current = adapter.models.first().cloned().unwrap_or_default();
                Some(
                    serde_json::json!({
                        "current_model_id": current,
                        "current_model_label": current,
                        "available_models": adapter.models.iter().map(|model| {
                            serde_json::json!({ "id": model, "label": model })
                        }).collect::<Vec<_>>(),
                    })
                    .to_string(),
                )
            };
            let available_modes_json = serde_json::json!({
                "current_mode_id": "full-access",
                "available_modes": [
                    {
                        "id": "full-access",
                        "name": "Full Access",
                        "description": "Codex can edit files and run commands without approval."
                    }
                ]
            })
            .to_string();

            let params = UpsertAgentMetadataParams {
                id: &adapter.id,
                icon: adapter.avatar.as_deref(),
                name: &adapter.name,
                name_i18n: None,
                description: adapter.description.as_deref(),
                description_i18n: None,
                backend: Some("codex-ollama"),
                agent_type: "acp",
                agent_source: "extension",
                agent_source_info: Some(&source_info_json),
                enabled: true,
                command: Some(command),
                args: Some(&args_json),
                env: Some(&env_json),
                native_skills_dirs: None,
                behavior_policy: None,
                yolo_id: Some("full-access"),
                agent_capabilities: None,
                auth_methods: None,
                config_options: None,
                available_modes: Some(&available_modes_json),
                available_models: available_models_json.as_deref(),
                available_commands: None,
                sort_order: 1450,
            };

            self.registry
                .repo_handle()
                .upsert(&params)
                .await
                .map_err(|e| AgentError::internal(format!("repo.upsert kodo adapter: {e}")))?;
            changed = true;
        }

        if changed {
            self.registry
                .invalidate_and_rehydrate()
                .await
                .map_err(|e| AgentError::internal(format!("registry rehydrate: {e}")))?;
        }
        Ok(())
    }
}
