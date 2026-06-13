use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use aionui_runtime::Builder as CmdBuilder;
use aionui_runtime::resolve_command_path;
use serde::Deserialize;
use tracing::warn;

use crate::types::ResolvedAcpAdapter;

const DEFAULT_KODO_PATH: &str = "/Users/richard/.local/bin/kodo";
const KODO_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KodoAionAdaptersResponse {
    status: String,
    #[serde(default)]
    adapters: Vec<KodoAionAdapter>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KodoAionAdapter {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    cli_command: Option<String>,
    #[serde(default)]
    default_cli_path: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    supports_streaming: Option<bool>,
    #[serde(default)]
    connection_type: Option<String>,
    #[serde(default)]
    yolo_mode: Option<serde_json::Value>,
    #[serde(default)]
    model: Option<String>,
}

pub async fn discover_kodo_acp_adapters() -> Vec<ResolvedAcpAdapter> {
    if std::env::var("AION_DISABLE_KODO_DISCOVERY").is_ok_and(|value| value == "1" || value == "true") {
        return Vec::new();
    }

    let Some(kodo) = resolve_kodo_command() else {
        return Vec::new();
    };

    let mut builder = CmdBuilder::clean_cli(&kodo);
    builder.args(["lanes", "aion-adapters", "--json"]);
    let output = match tokio::time::timeout(KODO_DISCOVERY_TIMEOUT, builder.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            warn!("Kodo ACP adapter discovery failed to start: {error}");
            return Vec::new();
        }
        Err(_) => {
            warn!("Kodo ACP adapter discovery timed out");
            return Vec::new();
        }
    };

    if !output.status.success() {
        warn!(status = ?output.status.code(), "Kodo ACP adapter discovery exited unsuccessfully");
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    match adapters_from_kodo_json(&stdout) {
        Ok(adapters) => adapters,
        Err(error) => {
            warn!("Kodo ACP adapter discovery returned invalid JSON: {error}");
            Vec::new()
        }
    }
}

fn resolve_kodo_command() -> Option<String> {
    if let Ok(path) = std::env::var("AION_KODO_CLI_PATH")
        && !path.trim().is_empty()
    {
        return Some(path);
    }
    if Path::new(DEFAULT_KODO_PATH).exists() {
        return Some(DEFAULT_KODO_PATH.to_string());
    }
    resolve_command_path("kodo").map(|path| path.to_string_lossy().to_string())
}

fn adapters_from_kodo_json(raw: &str) -> Result<Vec<ResolvedAcpAdapter>, serde_json::Error> {
    let response: KodoAionAdaptersResponse = serde_json::from_str(raw)?;
    if response.status != "ok" {
        return Ok(Vec::new());
    }
    Ok(response
        .adapters
        .into_iter()
        .map(|adapter| {
            let default_cli_path = adapter
                .default_cli_path
                .clone()
                .or_else(|| adapter.command.clone())
                .or_else(|| adapter.cli_command.clone());
            ResolvedAcpAdapter {
                extension_name: "kodo-lane-registry".to_string(),
                id: adapter.id,
                name: adapter.name,
                description: adapter.description,
                cli_command: adapter.cli_command.or_else(|| Some("kodo".to_string())),
                default_cli_path,
                acp_args: adapter.args,
                env: HashMap::new(),
                avatar: None,
                auth_required: Some(false),
                supports_streaming: adapter.supports_streaming,
                connection_type: adapter.connection_type,
                endpoint: None,
                models: adapter.model.into_iter().collect(),
                yolo_mode: adapter.yolo_mode,
                health_check: None,
                api_key_fields: vec![],
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kodo_lane_registry_adapter_output() {
        let adapters = adapters_from_kodo_json(
            r#"{
              "schema": "kodo.cli.lanes.aion-adapters.v1",
              "status": "ok",
              "adapters": [
                {
                  "id": "kodo-codex-ollama",
                  "name": "Kodo Codex Ollama Qwen3 30B Private",
                  "source": "kodo-lane-registry",
                  "laneId": "codex-ollama",
                  "cliCommand": "kodo",
                  "defaultCliPath": "/Users/richard/.local/bin/kodo",
                  "command": "/Users/richard/.local/bin/kodo",
                  "args": ["acp", "--backend", "codex-ollama"],
                  "agentType": "acp",
                  "backend": "codex-ollama",
                  "connectionType": "cli",
                  "supportsStreaming": true,
                  "yoloMode": { "type": "session" },
                  "privacy": "local_private",
                  "model": "qwen3-coder:30b-ctx64k"
                }
              ],
              "blockers": []
            }"#,
        )
        .unwrap();

        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].extension_name, "kodo-lane-registry");
        assert_eq!(adapters[0].id, "kodo-codex-ollama");
        assert_eq!(adapters[0].cli_command.as_deref(), Some("kodo"));
        assert_eq!(
            adapters[0].default_cli_path.as_deref(),
            Some("/Users/richard/.local/bin/kodo")
        );
        assert_eq!(adapters[0].acp_args, ["acp", "--backend", "codex-ollama"]);
        assert_eq!(adapters[0].supports_streaming, Some(true));
        assert_eq!(adapters[0].connection_type.as_deref(), Some("cli"));
        assert_eq!(adapters[0].models, ["qwen3-coder:30b-ctx64k"]);
        assert_eq!(adapters[0].yolo_mode.as_ref().unwrap()["type"], "session");
    }

    #[test]
    fn ignores_non_ok_kodo_registry_response() {
        let adapters = adapters_from_kodo_json(r#"{"status":"failed","adapters":[]}"#).unwrap();
        assert!(adapters.is_empty());
    }
}
