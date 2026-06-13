use std::collections::HashMap;
use std::path::PathBuf;

use aionui_common::McpSource;
use serde_json::{Map, Value};

use crate::adapter::{DetectedServer, McpAgentAdapter};
use crate::error::McpError;
use crate::types::McpServerTransport;

use super::cli_helpers::is_cli_installed;

const CURSOR_AGENT_CLI: &str = "cursor-agent";
const CURSOR_AGENT_FALLBACK_CLI: &str = "agent";

/// MCP adapter for Cursor global `~/.cursor/mcp.json` config.
pub struct CursorAdapter;

#[async_trait::async_trait]
impl McpAgentAdapter for CursorAdapter {
    fn source(&self) -> McpSource {
        McpSource::Cursor
    }

    async fn is_installed(&self) -> Result<bool, McpError> {
        Ok(is_cli_installed(CURSOR_AGENT_CLI).await? || is_cli_installed(CURSOR_AGENT_FALLBACK_CLI).await?)
    }

    async fn detect_existing(&self) -> Result<Vec<DetectedServer>, McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CURSOR_AGENT_CLI.into()));
        }
        let path = cursor_config_path()?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| McpError::AgentOperationFailed(format!("read Cursor MCP config: {e}")))?;
        parse_cursor_mcp_json(&content)
    }

    async fn install_server(&self, name: &str, transport: &McpServerTransport) -> Result<(), McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CURSOR_AGENT_CLI.into()));
        }
        let path = cursor_config_path()?;
        let mut config = read_cursor_config(&path).await?;
        let servers = ensure_mcp_servers_object(&mut config)?;
        servers.insert(name.to_owned(), transport_to_cursor_json(transport));
        write_cursor_config(&path, &config).await
    }

    async fn remove_server(&self, name: &str) -> Result<(), McpError> {
        if !self.is_installed().await? {
            return Err(McpError::AgentNotInstalled(CURSOR_AGENT_CLI.into()));
        }
        let path = cursor_config_path()?;
        if !path.exists() {
            return Ok(());
        }
        let mut config = read_cursor_config(&path).await?;
        if let Some(servers) = config.get_mut("mcpServers").and_then(Value::as_object_mut) {
            servers.remove(name);
        }
        write_cursor_config(&path, &config).await
    }
}

fn cursor_config_path() -> Result<PathBuf, McpError> {
    dirs::home_dir()
        .map(|home| home.join(".cursor").join("mcp.json"))
        .ok_or_else(|| McpError::AgentOperationFailed("cannot determine home directory".into()))
}

async fn read_cursor_config(path: &PathBuf) -> Result<Value, McpError> {
    if !path.exists() {
        return Ok(serde_json::json!({ "mcpServers": {} }));
    }
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| McpError::AgentOperationFailed(format!("read Cursor MCP config: {e}")))?;
    serde_json::from_str(&content).map_err(McpError::from)
}

async fn write_cursor_config(path: &PathBuf, config: &Value) -> Result<(), McpError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| McpError::AgentOperationFailed(format!("create Cursor MCP config dir: {e}")))?;
    }
    let content = serde_json::to_string_pretty(config).map_err(McpError::from)?;
    tokio::fs::write(path, format!("{content}\n"))
        .await
        .map_err(|e| McpError::AgentOperationFailed(format!("write Cursor MCP config: {e}")))
}

fn ensure_mcp_servers_object(config: &mut Value) -> Result<&mut Map<String, Value>, McpError> {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let root = config
        .as_object_mut()
        .ok_or_else(|| McpError::AgentOperationFailed("Cursor MCP config root is not an object".into()))?;
    let servers = root
        .entry("mcpServers".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    servers
        .as_object_mut()
        .ok_or_else(|| McpError::AgentOperationFailed("Cursor mcpServers is not an object".into()))
}

fn transport_to_cursor_json(transport: &McpServerTransport) -> Value {
    match transport {
        McpServerTransport::Stdio { command, args, env } => {
            let mut value = serde_json::json!({ "type": "stdio", "command": command, "args": args });
            if !env.is_empty() {
                value["env"] = serde_json::json!(env);
            }
            value
        }
        McpServerTransport::Http { url, headers } => http_like_to_cursor_json("http", url, headers),
        McpServerTransport::Sse { url, headers } => http_like_to_cursor_json("sse", url, headers),
    }
}

fn http_like_to_cursor_json(transport_type: &str, url: &str, headers: &HashMap<String, String>) -> Value {
    let mut value = serde_json::json!({ "type": transport_type, "url": url });
    if !headers.is_empty() {
        value["headers"] = serde_json::json!(headers);
    }
    value
}

fn parse_cursor_mcp_json(content: &str) -> Result<Vec<DetectedServer>, McpError> {
    let value: Value = serde_json::from_str(content).map_err(McpError::from)?;
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    Ok(servers
        .iter()
        .filter_map(|(name, entry)| parse_cursor_server(name, entry))
        .collect())
}

fn parse_cursor_server(name: &str, entry: &Value) -> Option<DetectedServer> {
    let transport_type = entry.get("type").and_then(Value::as_str).unwrap_or_else(|| {
        if entry.get("command").is_some() {
            "stdio"
        } else {
            "http"
        }
    });
    let transport = match transport_type {
        "stdio" => McpServerTransport::Stdio {
            command: entry.get("command")?.as_str()?.to_owned(),
            args: string_array(entry.get("args")),
            env: string_map(entry.get("env")),
        },
        "http" | "streamable_http" => McpServerTransport::Http {
            url: entry.get("url")?.as_str()?.to_owned(),
            headers: string_map(entry.get("headers")),
        },
        "sse" => McpServerTransport::Sse {
            url: entry.get("url")?.as_str()?.to_owned(),
            headers: string_map(entry.get("headers")),
        },
        _ => return None,
    };
    Some(DetectedServer {
        name: name.to_owned(),
        transport,
        importable: true,
        import_skip_reason: None,
    })
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn string_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_cursor() {
        assert_eq!(CursorAdapter.source(), McpSource::Cursor);
    }

    #[test]
    fn parses_stdio_cursor_config() {
        let servers = parse_cursor_mcp_json(
            r#"{
              "mcpServers": {
                "ghidra-mcp": {
                  "type": "stdio",
                  "command": "node",
                  "args": ["bridge.mjs"],
                  "env": { "K": "V" }
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "ghidra-mcp");
        match &servers[0].transport {
            McpServerTransport::Stdio { command, args, env } => {
                assert_eq!(command, "node");
                assert_eq!(args, &vec!["bridge.mjs".to_owned()]);
                assert_eq!(env.get("K").map(String::as_str), Some("V"));
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn writes_stdio_cursor_shape() {
        let value = transport_to_cursor_json(&McpServerTransport::Stdio {
            command: "node".into(),
            args: vec!["bridge.mjs".into()],
            env: HashMap::from([("K".into(), "V".into())]),
        });
        assert_eq!(value["type"], "stdio");
        assert_eq!(value["command"], "node");
        assert_eq!(value["args"], serde_json::json!(["bridge.mjs"]));
        assert_eq!(value["env"]["K"], "V");
    }
}
