pub mod assets;
mod auth;
#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod builtin_tests;
pub mod routes;
pub mod views;

use crate::state::AppState;
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderValue, header},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, patch, post},
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/", get(routes::index))
        .route("/c/{id}", get(routes::chat_page))
        .route("/healthz", get(routes::healthz))
        .route("/style.css", get(assets::style_css))
        .route("/app.js", get(assets::app_js))
        .route("/icon.svg", get(assets::icon_svg))
        .route("/manifest.webmanifest", get(assets::manifest))
        .route("/api/preferences", post(routes::update_preferences))
        .route("/api/tools", post(routes::update_tool))
        .route(
            "/api/schedules",
            get(routes::list_schedules).post(routes::add_schedule),
        )
        .route("/api/schedules/{id}", delete(routes::delete_schedule))
        .route("/api/mcp", post(routes::update_mcp))
        .route("/api/chats", post(routes::create_chat))
        .route(
            "/api/chats/{id}",
            patch(routes::patch_chat).delete(routes::delete_chat),
        )
        .route(
            "/api/chats/{id}/files",
            get(routes::list_files)
                .post(routes::upload_files)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/chats/{id}/files/{name}",
            get(routes::download_file).delete(routes::delete_file),
        )
        .route("/api/chats/{id}/answer", post(routes::answer_question))
        .route("/api/chats/{id}/compact", post(routes::compact_chat))
        .route("/api/chats/{id}/fork", post(routes::fork_chat))
        .route(
            "/api/folders",
            post(routes::create_folder)
                .patch(routes::rename_folder)
                .delete(routes::delete_folder),
        )
        .route("/api/stream", post(routes::stream_chat))
        .layer(middleware::from_fn(private_responses))
        .with_state(state)
}

async fn private_responses(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: http:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    ));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Secrets},
        storage::new_id,
    };
    use axum::{
        Json,
        extract::State as AxumState,
        http::{HeaderMap, HeaderValue, StatusCode, header},
        response::IntoResponse,
    };
    use serde_json::{Value, json};
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::net::TcpListener;

    fn build_multipart(parts: &[(&str, &str, &[u8])]) -> (String, Vec<u8>) {
        let boundary = "sunboundary7f8e9d";
        let mut body = Vec::new();
        for (filename, mime, data) in parts {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    async fn mock_mcp_post(headers: HeaderMap, Json(rpc): Json<Value>) -> Response {
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != "Bearer mcp-dyn-secret" {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let id = rpc.get("id").cloned().unwrap_or(Value::Null);
        let method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
        let body = match method {
            "server/discover" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" }
            }),
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "ops-mcp", "version": "1.0.0" }
                }
            }),
            "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "cluster.health",
                        "description": "Check cluster status",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "dc": { "type": "string" } }
                        }
                    }]
                }
            }),
            "tools/call" => {
                let dc = rpc["params"]["arguments"]["dc"]
                    .as_str()
                    .unwrap_or("unknown");
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!("CLUSTER_OK_{dc}") }],
                        "isError": false
                    }
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Unknown method" }
            }),
        };
        (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                ),
                (
                    header::HeaderName::from_static("mcp-session-id"),
                    HeaderValue::from_static("sess-1"),
                ),
            ],
            Json(body),
        )
            .into_response()
    }

    #[tokio::test]
    async fn end_to_end_chat_files_vision_xberg_mcp_and_hot_reload() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);

        let mock_upstream = Router::new()
            .route(
                "/mcp",
                post(mock_mcp_post)
                    .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                    .delete(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/models",
                get(|| async {
                    Json(json!({
                        "data": [{ "id": "discovered-model" }]
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(
                    move |headers: HeaderMap,
                          AxumState(store): AxumState<Arc<Mutex<Vec<Value>>>>,
                          Json(body): Json<Value>| async move {
                        let auth = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        assert_eq!(auth, "Bearer test-secret-key");
                        let has_mcp_tool = body.get("tools").and_then(Value::as_array).is_some_and(|arr| {
                            arr.iter().any(|t| {
                                t["function"]["name"] == "mcp__ops__cluster_health"
                                    || t["name"] == "mcp__ops__cluster_health"
                            })
                        });
                        let has_tool_result = body["messages"]
                            .as_array()
                            .is_some_and(|msgs| msgs.iter().any(|m| m["role"] == "tool"));
                        store.lock().expect("lock").push(body);

                        let sse = if has_mcp_tool && !has_tool_result {
                            concat!(
                                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"mcp__ops__cluster_health\",\"arguments\":\"{\\\"dc\\\":\\\"eu1\\\"}\"}}]}}]}\n\n",
                                "data: [DONE]\n\n"
                            )
                        } else {
                            concat!(
                                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step1\"}}]}\n\n",
                                "data: {\"choices\":[{\"delta\":{\"content\":\"Result $x^2$ \\n```rust\\nlet y = 1;\\n```\"}}]}\n\n",
                                "data: [DONE]\n\n"
                            )
                        };
                        ([(header::CONTENT_TYPE, "text/event-stream")], sse)
                    },
                ),
            )
            .with_state(captured_clone);

        let llm_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let llm_addr = llm_listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(llm_listener, mock_upstream).await;
        });

        let work_dir = PathBuf::from(format!("/tmp/opencode/sun-e2e-{}", new_id()));
        tokio::fs::create_dir_all(&work_dir).await.expect("mkdir");
        let cfg_path = work_dir.join("config.toml");
        let data_dir = work_dir.join("data");

        let initial_toml = format!(
            r##"[server]
bind = "127.0.0.1:0"
data_dir = "{}"

[branding]
name = "OpsOne"
accent_color = "#11aa55"
background_color = "#141018"
panel_color = "#211a26"

[files]
max_file_size = 512

[prompts]
developer_preprompt = "DEV_PRE"
system_prompt = "GLOBAL_SYS"

[llm]
base_url = "http://{}/v1"
auto_discover_models = true
default_model = "fast-1"

[[llm.models]]
id = "fast-1"
name = "Fast One"
reasoning_tags = ["low", "high"]
default_reasoning = "high"

[llm.models.tags]
smartness = "high"
pace = "fast"
price = "$"

[[mcp]]
name = "ops"
url = "http://{}/mcp"
enabled_by_default = false
"##,
            data_dir.display(),
            llm_addr,
            llm_addr
        );
        tokio::fs::write(&cfg_path, &initial_toml)
            .await
            .expect("write cfg");

        let cfg = Config::load(&cfg_path).await.expect("load cfg");
        let secrets = Secrets {
            llm_api_key: Some("test-secret-key".to_owned()),
            oidc_client_secret: None,
        };
        let state = AppState::new(cfg_path.clone(), cfg, secrets).expect("state");
        state.refresh_discovered_models().await;
        state.spawn_config_watcher();

        let app_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind app");
        let app_addr = app_listener.local_addr().expect("app addr");
        tokio::spawn(async move {
            let _ = axum::serve(app_listener, router(state)).await;
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client");
        let base = format!("http://{app_addr}");

        let home_html = client
            .get(format!("{base}/"))
            .send()
            .await
            .expect("get /")
            .text()
            .await
            .expect("text");
        assert!(home_html.contains("OpsOne"));
        assert!(home_html.contains("--accent: #11aa55; --bg: #141018; --panel: #211a26;"));
        assert!(home_html.contains("name=\"theme-color\" content=\"#141018\""));
        assert!(home_html.contains("rel=\"icon\" href=\"/icon.svg\""));
        assert!(home_html.contains("class=\"brand-logo\" src=\"/icon.svg\""));
        let icon = client
            .get(format!("{base}/icon.svg"))
            .send()
            .await
            .expect("icon");
        assert_eq!(
            icon.headers()[header::CONTENT_TYPE],
            "image/svg+xml; charset=utf-8"
        );
        let svg = icon.text().await.expect("SVG");
        assert!(svg.contains("stroke=\"#11aa55\""));
        assert!(svg.contains("fill=\"#211a26\""));
        assert!(home_html.contains("discovered-model"));
        assert!(home_html.contains("data-mcp=\"ops\""));
        let (chat_layout, runtime) = home_html
            .split_once("<aside id=\"runtime-panel\"")
            .expect("right panel");
        for id in [
            "model-select",
            "reasoning-select",
            "system-prompt",
            "shared-bar",
        ] {
            let attr = format!("id=\"{id}\"");
            assert!(!chat_layout.contains(&attr));
            assert!(runtime.contains(&attr));
        }
        assert!(runtime.contains(">Fast One</option>"));
        assert!(runtime.contains("data-tags="));
        assert!(runtime.contains("<label for=\"reasoning-select\">Reasoning</label><select"));
        let builtins = runtime
            .split_once("<summary>Built-in tools</summary>")
            .expect("built-in tools")
            .1
            .split_once("</details>")
            .expect("end of built-in tools")
            .0;
        assert!(builtins.contains("<button type=\"button\" id=\"compact-btn\""));

        let bad_model = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "model": "forbidden-model",
                "action": { "type": "send", "content": "hi" }
            }))
            .send()
            .await
            .expect("bad model");
        assert_eq!(bad_model.status(), StatusCode::BAD_REQUEST);

        let stream_text = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "system_prompt": "CHAT_SYS",
                "model": "fast-1",
                "reasoning": "low",
                "action": { "type": "send", "content": "First question" }
            }))
            .send()
            .await
            .expect("stream send")
            .text()
            .await
            .expect("ndjson");

        let events: Vec<Value> = stream_text
            .lines()
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect();
        assert_eq!(events[0]["model_name"], "Fast One");
        let chat_id = events[0]["chat_id"].as_str().expect("chat_id").to_owned();
        let user_msg_id = events[0]["user_msg"]["id"]
            .as_str()
            .expect("user_msg_id")
            .to_owned();
        let done = events
            .iter()
            .find(|e| e["type"] == "done")
            .expect("done event");
        assert!(done["html"].as_str().unwrap_or("").contains("<math"));
        assert!(done["html"].as_str().unwrap_or("").contains("copy-code"));
        assert_eq!(done["reasoning"], "step1");

        let oversized = vec![b'x'; 700];
        let (ct_big, body_big) = build_multipart(&[("huge.txt", "text/plain", &oversized)]);
        let big_res = client
            .post(format!("{base}/api/chats/{chat_id}/files"))
            .header(header::CONTENT_TYPE, ct_big)
            .body(body_big)
            .send()
            .await
            .expect("upload big");
        assert_eq!(big_res.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let png_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";
        let yml_bytes = b"cluster: prod-eu-1\nworkers: 12\n";
        let bin_bytes = b"\x00\xff\x10\x80custom-firmware";
        let (ct_ok, body_ok) = build_multipart(&[
            ("pixel.png", "image/png", png_bytes),
            ("infra.yml", "application/yaml", yml_bytes),
            ("fw.bin", "application/octet-stream", bin_bytes),
        ]);
        let up_res = client
            .post(format!("{base}/api/chats/{chat_id}/files"))
            .header(header::CONTENT_TYPE, ct_ok)
            .body(body_ok)
            .send()
            .await
            .expect("upload ok");
        assert_eq!(up_res.status(), StatusCode::OK);
        let uploaded_list: Vec<Value> = up_res.json().await.expect("uploaded json");
        assert_eq!(uploaded_list.len(), 3);
        let chat_html = client
            .get(format!("{base}/c/{chat_id}"))
            .send()
            .await
            .expect("chat with shared files")
            .text()
            .await
            .expect("chat HTML");
        assert!(chat_html.contains(&format!(
            ">infra.yml</a><span class=\"tag\">{} B</span>",
            yml_bytes.len()
        )));
        assert!(chat_html.contains(">Fork here</button>"));
        assert!(!chat_html.contains(">Fork chat</button>"));

        let dl_res = client
            .get(format!("{base}/api/chats/{chat_id}/files/fw.bin"))
            .send()
            .await
            .expect("download fw.bin");
        assert_eq!(dl_res.status(), StatusCode::OK);
        assert_eq!(
            dl_res
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(dl_res.bytes().await.expect("dl bytes").as_ref(), bin_bytes);

        let multimodal_stream = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "chat_id": chat_id,
                "model": "fast-1",
                "action": {
                    "type": "send",
                    "content": "Inspect these attachments",
                    "attachments": ["pixel.png", "infra.yml"]
                }
            }))
            .send()
            .await
            .expect("multimodal send")
            .text()
            .await
            .expect("drain multimodal");
        assert!(multimodal_stream.contains("\"type\":\"done\""));

        assert_eq!(
            client
                .post(format!("{base}/api/chats/{chat_id}/fork"))
                .json(&json!({}))
                .send()
                .await
                .expect("request without a message")
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let fork: Value = client
            .post(format!("{base}/api/chats/{chat_id}/fork"))
            .json(&json!({"through": user_msg_id}))
            .send()
            .await
            .expect("fork request")
            .error_for_status()
            .expect("forked")
            .json()
            .await
            .expect("fork summary");
        let fork_id = fork["id"].as_str().expect("fork id");
        assert_ne!(fork_id, chat_id);
        let fork_page = client
            .get(format!("{base}/c/{fork_id}"))
            .send()
            .await
            .expect("fork page")
            .text()
            .await
            .expect("fork markup");
        assert!(fork_page.contains("First question"));
        assert!(!fork_page.contains("Inspect these attachments"));
        let copied = client
            .get(format!("{base}/api/chats/{fork_id}/files/fw.bin"))
            .send()
            .await
            .expect("copied file")
            .bytes()
            .await
            .expect("bytes");
        assert_eq!(copied.as_ref(), bin_bytes);

        let edit_text = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "chat_id": chat_id,
                "model": "fast-1",
                "reasoning": "high",
                "action": {
                    "type": "edit",
                    "message_id": user_msg_id,
                    "content": "Edited question"
                }
            }))
            .send()
            .await
            .expect("edit send")
            .text()
            .await
            .expect("edit ndjson");
        let edit_events: Vec<Value> = edit_text
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        let assistant_id = edit_events
            .iter()
            .find(|e| e["type"] == "done")
            .and_then(|e| e["id"].as_str())
            .expect("assistant_id")
            .to_owned();

        let retry_res = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "chat_id": chat_id,
                "model": "fast-1",
                "action": { "type": "retry", "message_id": assistant_id }
            }))
            .send()
            .await
            .expect("retry");
        assert_eq!(retry_res.status(), StatusCode::OK);
        let _ = retry_res.text().await.expect("drain retry");

        let second_chat: Value = client
            .post(format!("{base}/api/chats"))
            .json(&json!({ "enabled_mcp": [] }))
            .send()
            .await
            .expect("create chat with legacy MCP settings")
            .json()
            .await
            .expect("second chat");
        let second_chat_id = second_chat["id"].as_str().expect("second chat id");
        assert_eq!(
            client
                .post(format!("{base}/api/mcp"))
                .json(&json!({
                    "name": "ops",
                    "enabled": true,
                    "auth_key": "Bearer mcp-dyn-secret"
                }))
                .send()
                .await
                .expect("enable mcp")
                .status(),
            StatusCode::NO_CONTENT
        );
        let second_page = client
            .get(format!("{base}/c/{second_chat_id}"))
            .send()
            .await
            .expect("second chat page")
            .text()
            .await
            .expect("second page HTML");
        assert!(second_page.contains("class=\"mcp-toggle\" data-mcp=\"ops\" checked"));

        let mcp_chat_stream = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "chat_id": second_chat_id,
                "model": "fast-1",
                "action": { "type": "send", "content": "Check eu1 cluster via MCP" }
            }))
            .send()
            .await
            .expect("mcp chat")
            .text()
            .await
            .expect("drain mcp chat");
        assert!(mcp_chat_stream.contains("Calling mcp__ops__cluster_health…"));
        assert!(mcp_chat_stream.contains("\"type\":\"continue\""));
        assert!(mcp_chat_stream.contains("\"output\":\"CLUSTER_OK_eu1\""));
        assert!(mcp_chat_stream.contains("\"type\":\"done\""));

        assert_eq!(
            client
                .patch(format!("{base}/api/chats/{second_chat_id}"))
                .json(&json!({ "model": "discovered-model" }))
                .send()
                .await
                .expect("switch model")
                .status(),
            StatusCode::OK
        );
        let refreshed = client
            .get(format!("{base}/c/{second_chat_id}"))
            .send()
            .await
            .expect("reload second chat")
            .text()
            .await
            .expect("second chat HTML");
        assert!(refreshed.contains("Assistant<span class=\"msg-model\"> (Fast One)</span>"));
        assert!(refreshed.contains("<summary>Reasoning</summary>"));
        assert!(refreshed.contains("<summary>Tool: mcp__ops__cluster_health</summary>"));
        assert!(refreshed.contains("class=\"activity-text tool-output\">CLUSTER_OK_eu1</pre>"));

        let second_files: Vec<Value> = client
            .get(format!("{base}/api/chats/{second_chat_id}/files"))
            .send()
            .await
            .expect("list second files")
            .json()
            .await
            .expect("second files json");
        assert!(second_files.is_empty());

        assert_eq!(
            client
                .delete(format!("{base}/api/chats/{chat_id}/files/fw.bin"))
                .send()
                .await
                .expect("delete fw.bin")
                .status(),
            StatusCode::NO_CONTENT
        );

        {
            let calls = captured.lock().expect("lock");
            assert_eq!(calls.len(), 6);
            assert_eq!(calls[0]["reasoning_effort"], "low");
            assert_eq!(
                calls[0]["messages"][0]["content"],
                "DEV_PRE\n\nGLOBAL_SYS\n\nCHAT_SYS"
            );

            let mm_payload = serde_json::to_string(&calls[1]["messages"]).expect("mm ser");
            assert!(mm_payload.contains("data:image/png;base64,"));
            assert!(mm_payload.contains("prod-eu-1"));

            assert_eq!(
                calls[4]["tools"][0]["function"]["name"],
                "mcp__ops__cluster_health"
            );
            let sixth_msgs = serde_json::to_string(&calls[5]["messages"]).expect("ser");
            assert!(sixth_msgs.contains("CLUSTER_OK_eu1"));
            assert!(!sixth_msgs.contains("Edited question"));
            assert!(!sixth_msgs.contains("prod-eu-1"));
        }

        assert_eq!(
            client
                .post(format!("{base}/api/folders"))
                .json(&json!({ "name": "infra" }))
                .send()
                .await
                .expect("mk folder")
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            client
                .patch(format!("{base}/api/chats/{chat_id}"))
                .json(&json!({ "folder": "infra", "title": "Renamed Chat" }))
                .send()
                .await
                .expect("patch chat")
                .status(),
            StatusCode::OK
        );

        let reloaded_toml = format!(
            r##"[server]
bind = "127.0.0.1:0"
data_dir = "{}"

[branding]
name = "HotReloadedBrand"
logo = "🌞"
accent_color = "#ff5500"
background_color = "#123"
panel_color = "#234"

[auth]
enabled = true
issuer_url = "https://idp.example.com"
client_id = "sun-web"
redirect_url = "https://sun.example.com/auth/callback"

[llm]
base_url = "http://{}/v1"
"##,
            data_dir.display(),
            llm_addr
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::fs::write(&cfg_path, reloaded_toml)
            .await
            .expect("rewrite cfg");
        tokio::time::sleep(Duration::from_millis(700)).await;

        let manifest_json: Value = client
            .get(format!("{base}/manifest.webmanifest"))
            .send()
            .await
            .expect("manifest")
            .json()
            .await
            .expect("manifest json");
        assert_eq!(manifest_json["name"], "HotReloadedBrand");
        assert_eq!(manifest_json["theme_color"], "#ff5500");
        assert_eq!(manifest_json["background_color"], "#123");
        assert_eq!(manifest_json["icons"][0]["src"], "/icon.svg");
        let reloaded_icon = client
            .get(format!("{base}/icon.svg"))
            .send()
            .await
            .expect("reloaded icon")
            .text()
            .await
            .expect("SVG");
        assert!(reloaded_icon.contains(">🌞</text>"));
        assert!(reloaded_icon.contains("fill=\"#234\""));

        let auth_redirect = client
            .get(format!("{base}/"))
            .send()
            .await
            .expect("redirect check");
        assert_eq!(auth_redirect.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            auth_redirect
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/auth/login")
        );

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
    }

    #[tokio::test]
    async fn responses_api_supports_reasoning_with_mcp_tools() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);

        let mock_upstream = Router::new()
            .route(
                "/mcp",
                post(mock_mcp_post)
                    .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                    .delete(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/responses",
                post(
                    move |AxumState(store): AxumState<Arc<Mutex<Vec<Value>>>>,
                          Json(body): Json<Value>| async move {
                        let has_fn_output = body["input"]
                            .as_array()
                            .is_some_and(|arr| arr.iter().any(|i| i["type"] == "function_call_output"));
                        store.lock().expect("lock").push(body);

                        let sse = if !has_fn_output {
                            concat!(
                                "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking...\"}\n\n",
                                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[],\"encrypted_content\":\"ENC_STATE_1\"}}\n\n",
                                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_42\",\"name\":\"mcp__ops__cluster_health\",\"arguments\":\"{\\\"dc\\\":\\\"us1\\\"}\"}}\n\n",
                                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
                            )
                        } else {
                            concat!(
                                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Cluster status: healthy\"}\n\n",
                                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
                            )
                        };
                        ([(header::CONTENT_TYPE, "text/event-stream")], sse)
                    },
                ),
            )
            .with_state(captured_clone);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream_addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, mock_upstream).await;
        });

        let work_dir = PathBuf::from(format!("/tmp/opencode/sun-resp-{}", new_id()));
        tokio::fs::create_dir_all(&work_dir).await.expect("mkdir");
        let cfg_path = work_dir.join("config.toml");
        let data_dir = work_dir.join("data");

        let raw_toml = format!(
            r##"[server]
bind = "127.0.0.1:0"
data_dir = "{}"

[llm]
base_url = "http://{}/v1"
default_model = "gpt-6.1-sol"

[[llm.models]]
id = "gpt-6.1-sol"
reasoning_tags = ["none", "low", "medium", "high", "xhigh", "max"]
default_reasoning = "xhigh"

[[mcp]]
name = "ops"
url = "http://{}/mcp"
enabled_by_default = true
"##,
            data_dir.display(),
            upstream_addr,
            upstream_addr
        );
        tokio::fs::write(&cfg_path, raw_toml).await.expect("write");

        let cfg = Config::load(&cfg_path).await.expect("load");
        let secrets = Secrets {
            llm_api_key: Some("sk-test".to_owned()),
            oidc_client_secret: None,
        };
        let state = AppState::new(cfg_path, cfg, secrets).expect("state");

        let app_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind app");
        let app_addr = app_listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(app_listener, router(state)).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://{app_addr}");

        assert_eq!(
            client
                .post(format!("{base}/api/mcp"))
                .json(&json!({ "name": "ops", "auth_key": "mcp-dyn-secret" }))
                .send()
                .await
                .expect("set key")
                .status(),
            StatusCode::NO_CONTENT
        );

        let stream_out = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "model": "gpt-6.1-sol",
                "reasoning": "xhigh",
                "action": { "type": "send", "content": "Check us1 health" }
            }))
            .send()
            .await
            .expect("stream")
            .text()
            .await
            .expect("text");

        assert!(stream_out.contains("Cluster status: healthy"));
        let init: Value = serde_json::from_str(stream_out.lines().next().expect("init event"))
            .expect("init JSON");
        let chat_id = init["chat_id"].as_str().expect("chat id");
        let completed = client
            .get(format!("{base}/c/{chat_id}"))
            .send()
            .await
            .expect("reload finished chat")
            .text()
            .await
            .expect("chat HTML");
        assert!(completed.contains("<details class=\"activity reasoning\"><summary>Reasoning</summary><div class=\"activity-text\">thinking...</div>"));
        assert!(completed.contains("<summary>Tool: mcp__ops__cluster_health</summary>"));
        assert!(completed.contains("class=\"activity-text tool-output\">CLUSTER_OK_us1</pre>"));
        assert!(!completed.contains("ENC_STATE_1"));

        {
            let calls = captured.lock().expect("lock");
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["reasoning"]["effort"], "xhigh");
            assert_eq!(calls[0]["include"][0], "reasoning.encrypted_content");
            assert_eq!(calls[0]["tools"][0]["name"], "mcp__ops__cluster_health");

            let round2_input = serde_json::to_string(&calls[1]["input"]).expect("ser");
            assert!(round2_input.contains("ENC_STATE_1"));
            assert!(round2_input.contains("CLUSTER_OK_us1"));
        }

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
    }

    #[tokio::test]
    async fn builtin_tools_ask_user_shared_search_and_compaction() {
        use futures_util::StreamExt as _;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);

        let mock_upstream = Router::new()
            .route(
                "/v1/responses",
                post(
                    move |AxumState(store): AxumState<Arc<Mutex<Vec<Value>>>>,
                          Json(body): Json<Value>| async move {
                        let input_str = serde_json::to_string(&body["input"]).unwrap_or_default();
                        store.lock().expect("lock").push(body.clone());

                        let sse = if input_str.contains("Compact our entire conversation") {
                            concat!(
                                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Compacted summary: staging deployed with runbook.txt.\"}\n\n",
                                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
                            )
                        } else if input_str.contains("Seed alpha42runbook") {
                            concat!(
                                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Stored seed note.\"}\n\n",
                                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
                            )
                        } else if input_str.contains("Find alpha42runbook")
                            && !input_str.contains("function_call_output")
                        {
                            concat!(
                                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_s\",\"call_id\":\"call_search\",\"name\":\"search\",\"arguments\":\"{\\\"query\\\":\\\"alpha42runbook\\\"}\"}}\n\n",
                                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
                            )
                        } else if input_str.contains("Interactive deploy")
                            && !input_str.contains("function_call_output")
                        {
                            concat!(
                                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_t\",\"call_id\":\"call_ts\",\"name\":\"current_timestamp\",\"arguments\":\"{}\"}}\n\n",
                                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_w\",\"call_id\":\"call_wr\",\"name\":\"shared\",\"arguments\":\"{\\\"action\\\":\\\"write\\\",\\\"filename\\\":\\\"runbook.txt\\\",\\\"content\\\":\\\"step-ok\\\"}\"}}\n\n",
                                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_a\",\"call_id\":\"call_ask\",\"name\":\"ask_user\",\"arguments\":\"{\\\"question\\\":\\\"Target env?\\\",\\\"options\\\":[\\\"staging\\\",\\\"prod\\\"]}\"}}\n\n",
                                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
                            )
                        } else {
                            concat!(
                                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Done interactive flow.\"}\n\n",
                                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
                            )
                        };

                        ([(header::CONTENT_TYPE, "text/event-stream")], sse)
                    },
                ),
            )
            .with_state(captured_clone);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream_addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, mock_upstream).await;
        });

        let work_dir = PathBuf::from(format!("/tmp/opencode/sun-step4-{}", new_id()));
        tokio::fs::create_dir_all(&work_dir).await.expect("mkdir");
        let cfg_path = work_dir.join("config.toml");
        let data_dir = work_dir.join("data");

        let toml_without_search = format!(
            r##"[server]
bind = "127.0.0.1:0"
data_dir = "{}"

[llm]
base_url = "http://{}/v1"
default_model = "m1"

[[llm.models]]
id = "m1"
"##,
            data_dir.display(),
            upstream_addr
        );
        tokio::fs::write(&cfg_path, &toml_without_search)
            .await
            .expect("write cfg");

        let cfg = Config::load(&cfg_path).await.expect("load");
        let secrets = Secrets {
            llm_api_key: Some("sk-test".to_owned()),
            oidc_client_secret: None,
        };
        let state = AppState::new(cfg_path.clone(), cfg, secrets.clone()).expect("state");
        state.spawn_config_watcher();

        let app_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind app");
        let app_addr = app_listener.local_addr().expect("addr");
        let state_ref = state.clone();
        tokio::spawn(async move {
            let _ = axum::serve(app_listener, router(state_ref)).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://{app_addr}");

        let _ = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "model": "m1",
                "action": { "type": "send", "content": "Seed alpha42runbook in chat one" }
            }))
            .send()
            .await
            .expect("seed")
            .text()
            .await
            .expect("seed text");

        {
            let calls = captured.lock().expect("lock");
            let tool_names: Vec<&str> = calls[0]["tools"]
                .as_array()
                .expect("tools arr")
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect();
            assert!(tool_names.contains(&"current_timestamp"));
            assert!(tool_names.contains(&"shared"));
            assert!(tool_names.contains(&"ask_user"));
            assert!(!tool_names.contains(&"search"));
            assert!(!tool_names.contains(&"schedule_prompt") && !tool_names.contains(&"fork_chat"));
        }

        for (name, enabled) in [("current_timestamp", false), ("search", true)] {
            assert_eq!(
                client
                    .post(format!("{base}/api/tools"))
                    .json(&json!({ "name": name, "enabled": enabled }))
                    .send()
                    .await
                    .expect("save tool preference")
                    .status(),
                StatusCode::NO_CONTENT
            );
        }

        let resp = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "model": "m1",
                "action": { "type": "send", "content": "Interactive deploy" }
            }))
            .send()
            .await
            .expect("interactive send");

        let mut byte_stream = resp.bytes_stream();
        let mut buf = Vec::new();
        let mut chat_id = String::new();
        let mut answered = false;
        let mut saw_shared_updated = false;
        let mut saw_done = false;

        while let Some(chunk) = byte_stream.next().await {
            buf.extend_from_slice(&chunk.expect("chunk"));
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let s = String::from_utf8_lossy(&line);
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let ev: Value = serde_json::from_str(trimmed).expect("ev json");
                match ev["type"].as_str().unwrap_or("") {
                    "init" => {
                        chat_id = ev["chat_id"].as_str().expect("cid").to_owned();
                    }
                    "shared_updated" => saw_shared_updated = true,
                    "ask_user" => {
                        let qid = ev["question_id"].as_str().expect("qid").to_owned();
                        assert_eq!(ev["question"], "Target env?");
                        let ans_res = client
                            .post(format!("{base}/api/chats/{chat_id}/answer"))
                            .json(&json!({ "question_id": qid, "answer": "staging" }))
                            .send()
                            .await
                            .expect("post answer");
                        assert_eq!(ans_res.status(), StatusCode::NO_CONTENT);
                        answered = true;
                    }
                    "done" => {
                        assert!(
                            ev["raw"]
                                .as_str()
                                .unwrap_or("")
                                .contains("Done interactive flow.")
                        );
                        saw_done = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(answered);
        assert!(saw_shared_updated);
        assert!(saw_done);

        let written_file = client
            .get(format!("{base}/api/chats/{chat_id}/files/runbook.txt"))
            .send()
            .await
            .expect("dl runbook")
            .text()
            .await
            .expect("runbook body");
        assert_eq!(written_file, "step-ok");

        let updated_defaults = format!("{toml_without_search}\n[tools]\nshared = false\n");
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::fs::write(&cfg_path, updated_defaults)
            .await
            .expect("update tool defaults");
        tokio::time::sleep(Duration::from_millis(700)).await;

        let page = client
            .get(format!("{base}/c/{chat_id}"))
            .send()
            .await
            .expect("chat page")
            .text()
            .await
            .expect("chat HTML");
        assert!(page.contains("data-tool=\"search\" checked"));
        assert!(page.contains("data-tool=\"ask_user\" checked"));
        assert!(!page.contains("data-tool=\"current_timestamp\" checked"));
        assert!(!page.contains("data-tool=\"shared\" checked"));

        let _ = client
            .post(format!("{base}/api/stream"))
            .json(&json!({
                "chat_id": chat_id,
                "model": "m1",
                "action": { "type": "send", "content": "Find alpha42runbook" }
            }))
            .send()
            .await
            .expect("search turn")
            .text()
            .await
            .expect("drain search");

        let compact_res: Value = client
            .post(format!("{base}/api/chats/{chat_id}/compact"))
            .send()
            .await
            .expect("compact")
            .json()
            .await
            .expect("compact json");
        assert_eq!(compact_res["assistant_msg"]["model_name"], "m1");
        assert!(
            compact_res["assistant_msg"]["raw"]
                .as_str()
                .unwrap_or("")
                .contains("Compacted summary")
        );

        let compacted_chat = state
            .storage()
            .get_chat("local", &chat_id)
            .await
            .expect("get compacted chat");
        assert_eq!(compacted_chat.messages.len(), 2);

        {
            let calls = captured.lock().expect("lock");
            let all_inputs = serde_json::to_string(&*calls).expect("ser calls");
            assert!(all_inputs.contains("User answered: staging"));
            assert!(all_inputs.contains("Seed alpha42runbook in chat one"));
            assert!(all_inputs.contains("current_timestamp is not enabled"));
            let names: Vec<_> = calls[3]["tools"]
                .as_array()
                .expect("search tools")
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect();
            assert_eq!(names, ["ask_user", "search"]);
        }

        let _ = tokio::fs::remove_dir_all(&work_dir).await;
    }
}
