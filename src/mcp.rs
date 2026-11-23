use crate::{
    config::McpServerConfig,
    domain::{ToolSpec, UserState},
    error::AppError,
};
use futures_util::future::join_all;
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{
        CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ContentBlock,
        Implementation, JsonObject, ProtocolVersion, ResourceContents, Tool,
    },
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

type RunningClient = RunningService<RoleClient, ClientConfig>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SessionKey {
    user_id: String,
    server: String,
    url: String,
    auth_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct McpRoute {
    pub server: McpServerConfig,
    pub tool_name: String,
    pub auth_key: Option<String>,
}

#[derive(Default)]
pub struct McpHub {
    sessions: Mutex<HashMap<SessionKey, Arc<RunningClient>>>,
}

impl McpHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn evict_server(&self, user_id: &str, server_name: &str) {
        let mut guard = self.sessions.lock().await;
        guard.retain(|k, _| !(k.user_id == user_id && k.server == server_name));
    }

    pub async fn prune_stale_urls(&self, servers: &[McpServerConfig]) {
        let mut guard = self.sessions.lock().await;
        guard.retain(|k, svc| {
            !svc.is_closed()
                && servers
                    .iter()
                    .any(|s| s.name == k.server && s.url.trim() == k.url)
        });
    }

    async fn get_or_connect(&self, key: &SessionKey) -> Result<Arc<RunningClient>, AppError> {
        {
            let mut guard = self.sessions.lock().await;
            if let Some(existing) = guard.get(key) {
                if !existing.is_closed() {
                    return Ok(Arc::clone(existing));
                }
                guard.remove(key);
            }
        }

        let mut transport_cfg = StreamableHttpClientTransportConfig::with_uri(key.url.as_str());
        transport_cfg.allow_stateless = true;
        transport_cfg.reinit_on_expired_session = true;
        if let Some(token) = key.auth_key.as_deref() {
            transport_cfg = transport_cfg.auth_header(token);
        }

        let transport = StreamableHttpClientTransport::from_config(transport_cfg);
        let client_cfg = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        );

        let connected = tokio::time::timeout(
            Duration::from_secs(15),
            client_cfg.serve_with_lifecycle(
                transport,
                ClientLifecycleMode::Auto {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                    legacy_version: Some(ProtocolVersion::V_2025_11_25),
                },
            ),
        )
        .await
        .map_err(|_| AppError::Upstream(format!("mcp connection to '{}' timed out", key.server)))?
        .map_err(|e| AppError::Upstream(format!("mcp '{}' init error: {e}", key.server)))?;

        let arc = Arc::new(connected);
        self.sessions
            .lock()
            .await
            .insert(key.clone(), Arc::clone(&arc));
        Ok(arc)
    }

    async fn remove_exact(&self, key: &SessionKey) {
        self.sessions.lock().await.remove(key);
    }

    async fn call_with_retry<T, F, Fut, E>(
        &self,
        key: &SessionKey,
        op_name: &str,
        timeout: Duration,
        mut op: F,
    ) -> Result<T, AppError>
    where
        F: FnMut(Arc<RunningClient>) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        for attempt in 0..2 {
            let client = self.get_or_connect(key).await?;
            match tokio::time::timeout(timeout, op(client)).await {
                Ok(Ok(val)) => return Ok(val),
                Ok(Err(_)) if attempt == 0 => self.remove_exact(key).await,
                Ok(Err(e)) => {
                    self.remove_exact(key).await;
                    return Err(AppError::Upstream(format!(
                        "mcp '{}' {op_name} failed: {e}",
                        key.server
                    )));
                }
                Err(_) => {
                    self.remove_exact(key).await;
                    return Err(AppError::Upstream(format!(
                        "mcp '{}' {op_name} timed out",
                        key.server
                    )));
                }
            }
        }
        Err(AppError::Upstream(format!(
            "mcp '{}' {op_name} failed",
            key.server
        )))
    }

    async fn list_tools_for_server(
        &self,
        user_id: &str,
        server: &McpServerConfig,
        auth_key: Option<String>,
    ) -> Result<Vec<Tool>, AppError> {
        let key = SessionKey {
            user_id: user_id.to_owned(),
            server: server.name.clone(),
            url: server.url.trim().to_owned(),
            auth_key,
        };
        self.call_with_retry(
            &key,
            "list_tools",
            Duration::from_secs(15),
            |c| async move { c.list_all_tools().await },
        )
        .await
    }

    pub async fn discover_tools(
        &self,
        user_id: &str,
        servers: &[McpServerConfig],
        user_state: &UserState,
    ) -> Result<(Vec<ToolSpec>, HashMap<String, McpRoute>), AppError> {
        if servers.is_empty() {
            return Ok((Vec::new(), HashMap::new()));
        }

        let tasks = servers.iter().map(|srv| {
            let auth = user_state
                .mcp
                .get(&srv.name)
                .and_then(|s| clean_bearer(s.auth_key.as_deref()));
            async move {
                let tools = self
                    .list_tools_for_server(user_id, srv, auth.clone())
                    .await?;
                Ok::<_, AppError>((srv, auth, tools))
            }
        });

        let results = join_all(tasks).await;
        let mut specs = Vec::new();
        let mut routes = HashMap::new();

        for item in results {
            let (srv, auth, tools) = item?;
            for tool in tools {
                let fn_name = encode_tool_name(&srv.name, &tool.name);
                let desc = match tool.description.as_deref().filter(|d| !d.trim().is_empty()) {
                    Some(d) => format!("[MCP: {}] {d}", srv.name),
                    None => format!("[MCP: {}] {}", srv.name, tool.name),
                };
                let parameters = normalize_schema(&tool.input_schema);
                routes.insert(
                    fn_name.clone(),
                    McpRoute {
                        server: srv.clone(),
                        tool_name: tool.name.into_owned(),
                        auth_key: auth.clone(),
                    },
                );
                specs.push(ToolSpec {
                    name: fn_name,
                    description: desc,
                    parameters,
                });
            }
        }

        Ok((specs, routes))
    }

    pub async fn execute_tool(
        &self,
        user_id: &str,
        route: &McpRoute,
        raw_arguments: &str,
    ) -> Result<String, AppError> {
        let args = parse_arguments(raw_arguments)?;
        let key = SessionKey {
            user_id: user_id.to_owned(),
            server: route.server.name.clone(),
            url: route.server.url.trim().to_owned(),
            auth_key: route.auth_key.clone(),
        };
        let tool_name = route.tool_name.clone();
        let res = self
            .call_with_retry(&key, &tool_name, Duration::from_secs(60), |c| {
                let params =
                    CallToolRequestParams::new(tool_name.clone()).with_arguments(args.clone());
                async move { c.call_tool(params).await }
            })
            .await?;
        Ok(format_tool_result(res))
    }
}

pub fn clean_bearer(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = if trimmed.len() > 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    };
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

pub fn encode_tool_name(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let candidate = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if candidate.len() <= 64 {
        return candidate;
    }
    let mut hash: u32 = 0x811c_9dc5;
    for b in candidate.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{}_{hash:08x}", &candidate[..55])
}

fn normalize_schema(schema: &JsonObject) -> Value {
    let mut obj = schema.clone();
    if !obj.contains_key("type") {
        obj.insert("type".to_owned(), Value::String("object".to_owned()));
    }
    if obj.get("type").and_then(Value::as_str) == Some("object") && !obj.contains_key("properties")
    {
        obj.insert("properties".to_owned(), json!({}));
    }
    Value::Object(obj)
}

fn parse_arguments(raw: &str) -> Result<JsonObject, AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(JsonObject::new());
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(Value::Null) => Ok(JsonObject::new()),
        Ok(_) => Err(AppError::BadRequest(
            "tool arguments must be a JSON object".to_owned(),
        )),
        Err(e) => Err(AppError::BadRequest(format!(
            "invalid tool JSON arguments: {e}"
        ))),
    }
}

fn format_tool_result(res: CallToolResult) -> String {
    let mut parts = Vec::new();
    for block in res.content {
        match block {
            ContentBlock::Text(t) => parts.push(t.text),
            ContentBlock::Resource(emb) => match emb.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    parts.push(format!("[Resource {uri}]\n{text}"));
                }
                ResourceContents::BlobResourceContents { uri, mime_type, .. } => {
                    let mime = mime_type.unwrap_or_else(|| "application/octet-stream".to_owned());
                    parts.push(format!("[Binary resource {uri} ({mime})]"));
                }
                _ => {}
            },
            ContentBlock::ResourceLink(r) => {
                parts.push(format!("[Resource link: {} ({})]", r.name, r.uri));
            }
            ContentBlock::Image(img) => {
                parts.push(format!(
                    "[Image returned ({}, {} base64 chars)]",
                    img.mime_type,
                    img.data.len()
                ));
            }
            ContentBlock::Audio(aud) => {
                parts.push(format!("[Audio returned ({})]", aud.mime_type));
            }
            _ => {}
        }
    }

    if let Some(structured) = res.structured_content
        && !structured.is_null()
        && let Ok(serialized) = serde_json::to_string(&structured)
        && !parts.iter().any(|p| p.trim() == serialized)
    {
        parts.push(serialized);
    }

    let body = if parts.is_empty() {
        "ok".to_owned()
    } else {
        parts.join("\n\n")
    };

    if res.is_error == Some(true) {
        format!("Tool error:\n{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_openai_safe_names_and_strips_bearer_prefix() {
        let short = encode_tool_name("github", "issues.list");
        assert_eq!(short, "mcp__github__issues_list");

        let very_long = encode_tool_name(
            "observability-cluster",
            "query_prometheus_range_metrics_across_all_federated_namespaces_and_pods",
        );
        assert!(very_long.len() <= 64);
        assert!(
            very_long
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        );

        assert_eq!(
            clean_bearer(Some("Bearer my-secret-token")).as_deref(),
            Some("my-secret-token")
        );
        assert_eq!(
            clean_bearer(Some("  raw-token  ")).as_deref(),
            Some("raw-token")
        );
        assert!(clean_bearer(Some("   ")).is_none());
    }
}
