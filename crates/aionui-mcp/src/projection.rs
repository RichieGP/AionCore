use crate::session_injection::AcpMcpCapabilities;

/// Transport-aware decision for how Aion should expose a selected MCP server
/// to an agent lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpProjectionKind {
    /// The agent accepts this MCP transport in the ACP `session/new` payload.
    DirectSession,
    /// The agent should receive this MCP through its native MCP config.
    NativeConfig,
    /// Aion must proxy the selected MCP into a transport the agent supports.
    ProxyRequired,
    /// Aion has no known safe projection for this backend/transport pair.
    Unsupported,
}

/// Concrete projection decision plus the reason, kept small enough for logs and
/// future API diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProjectionDecision {
    pub kind: McpProjectionKind,
    pub reason: &'static str,
}

pub fn plan_mcp_projection(
    backend: Option<&str>,
    transport_type: &str,
    capabilities: &AcpMcpCapabilities,
) -> McpProjectionDecision {
    match transport_type {
        "stdio" if capabilities.stdio => decision(McpProjectionKind::DirectSession, "backend accepts stdio MCP"),
        "http" | "streamable_http" if capabilities.http => {
            decision(McpProjectionKind::DirectSession, "backend accepts streamable HTTP MCP")
        }
        "sse" if capabilities.sse => decision(McpProjectionKind::DirectSession, "backend accepts SSE MCP"),
        "stdio" if backend_supports_native_config_stdio(backend) => decision(
            McpProjectionKind::NativeConfig,
            "backend supports stdio through native MCP config",
        ),
        "stdio" if capabilities.http || capabilities.sse => decision(
            McpProjectionKind::ProxyRequired,
            "backend lacks stdio but accepts network MCP transport",
        ),
        _ => decision(McpProjectionKind::Unsupported, "no compatible MCP transport projection"),
    }
}

pub fn backend_supports_native_config_stdio(backend: Option<&str>) -> bool {
    matches!(
        backend,
        Some("cursor" | "qwen" | "codex" | "claude" | "gemini" | "aionrs")
    )
}

fn decision(kind: McpProjectionKind, reason: &'static str) -> McpProjectionDecision {
    McpProjectionDecision { kind, reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(stdio: bool, http: bool, sse: bool) -> AcpMcpCapabilities {
        AcpMcpCapabilities { stdio, http, sse }
    }

    #[test]
    fn stdio_capable_backend_uses_direct_session() {
        let plan = plan_mcp_projection(Some("cursor"), "stdio", &caps(true, true, true));
        assert_eq!(plan.kind, McpProjectionKind::DirectSession);
    }

    #[test]
    fn known_native_stdio_backend_can_fall_back_to_native_config() {
        let plan = plan_mcp_projection(Some("cursor"), "stdio", &caps(false, true, true));
        assert_eq!(plan.kind, McpProjectionKind::NativeConfig);
    }

    #[test]
    fn unknown_http_backend_requires_proxy_for_stdio() {
        let plan = plan_mcp_projection(Some("unknown"), "stdio", &caps(false, true, false));
        assert_eq!(plan.kind, McpProjectionKind::ProxyRequired);
    }

    #[test]
    fn http_server_uses_direct_http_when_supported() {
        let plan = plan_mcp_projection(Some("unknown"), "http", &caps(false, true, false));
        assert_eq!(plan.kind, McpProjectionKind::DirectSession);
    }

    #[test]
    fn unsupported_transport_reports_unsupported() {
        let plan = plan_mcp_projection(Some("unknown"), "stdio", &caps(false, false, false));
        assert_eq!(plan.kind, McpProjectionKind::Unsupported);
    }
}
