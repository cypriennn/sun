use super::*;
use crate::{
    config::{Config, Secrets},
    schedule::Scheduler,
    storage::new_id,
};
use axum::{Json, http::StatusCode};
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::net::TcpListener;

const TEST_SECRETS: Secrets = Secrets {
    llm_api_key: None,
    oidc_client_secret: None,
};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    (
        base,
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") }),
    )
}

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .map(|line| serde_json::from_str(line).expect("NDJSON"))
        .collect()
}

#[tokio::test]
async fn temporary_chat_is_private_and_location_requires_consent() {
    use futures_util::StreamExt as _;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let upstream = Router::new().route("/responses", post(|Json(body): Json<Value>| async move {
        let input = body["input"].to_string();
        let tools = body["tools"].to_string();
        assert!(tools.contains("current_location"));
        assert!(tools.contains("current_timestamp"));
        for disabled in ["schedule_prompt", "fork_chat", "search", "shared"] {
            assert!(!tools.contains(disabled));
        }
        let output = if input.contains("where am I") && !input.contains("function_call_output") && !input.contains("follow up") {
            assert!(!input.contains("Europe/Paris"));
            json!([
                {"type": "function_call", "id": "fc_time", "call_id": "time", "name": "current_timestamp", "arguments": "{}"},
                {"type": "function_call", "id": "fc_loc", "call_id": "loc", "name": "current_location", "arguments": "{}"},
            ])
        } else {
            if input.contains("follow up") {
                assert!(input.contains("where am I") && input.contains("test answer"));
            } else {
                let result = |id: &str| {
                    let item = body["input"].as_array().expect("input").iter().find(|item| item["type"] == "function_call_output" && item["call_id"] == id).expect("tool result");
                    serde_json::from_str::<Value>(item["output"].as_str().expect("output")).expect("tool JSON")
                };
                let time = result("time");
                assert_eq!(time["timezone"], "Europe/Paris");
                assert!(time["local"].as_str().expect("local time").ends_with("+02:00"));
                assert!(time["utc"].is_string() && time["latitude"].is_null());
                assert_eq!(result("loc"), json!({"latitude": 48.86, "longitude": 2.35}));
            }
            json!([{"type": "message", "content": [{"type": "output_text", "text": "test answer"}]}])
        };
        Json(json!({"output": output, "usage": {"total_tokens": 5}}))
    }));
    let (upstream, provider) = serve(upstream).await;
    let dir = PathBuf::from(format!("/tmp/opencode/sun-temporary-{}", new_id()));
    let raw = format!(
        r#"
[server]
data_dir = "{}"
[llm]
base_url = "{upstream}"
[[llm.models]]
id = "test-model"
[tools]
current_location = true
search = true
"#,
        dir.display()
    );
    let state = AppState::new(
        dir.join("config.toml"),
        Config::parse_toml(&raw).expect("config"),
        TEST_SECRETS,
    )
    .expect("state");
    let (base, app) = serve(router(state.clone())).await;
    let client = reqwest::Client::new();
    let mut body = client
        .post(format!("{base}/api/stream"))
        .json(&json!({
            "temporary": [], "action": {"type": "send", "content": "where am I and what time is it?"},
            "browser_time_zone": {"timezone": "Europe/Paris", "utc_offset_minutes": 120},
        }))
        .send()
        .await
        .expect("stream")
        .error_for_status()
        .expect("status")
        .bytes_stream();
    let mut response = String::new();
    let mut replied = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = body.next().await {
            response.push_str(std::str::from_utf8(&chunk.expect("chunk")).expect("UTF-8"));
            if replied { continue; }
            let mut received = response.split_inclusive('\n').filter(|line| line.ends_with('\n'))
                .map(|line| serde_json::from_str::<Value>(line).expect("event"));
            let Some(init) = received.next() else { continue };
             if let Some(request) = received.find(|event| event["type"] == "current_location") {
                 assert!(response.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).any(|event| event["type"] == "tool_end" && event["output"].as_str().is_some_and(|out| out.contains("Europe/Paris"))));
                let id = init["chat_id"].as_str().expect("id");
                client.post(format!("{base}/api/chats/{id}/answer")).json(&json!({
                    "question_id": request["question_id"],
                    "answer": json!({"timezone": "Europe/Paris", "utc_offset_minutes": 60, "latitude": 48.8566, "longitude": 2.3522}).to_string(),
                })).send().await.expect("reply").error_for_status().expect("accepted");
                replied = true;
            }
        }
    }).await.expect("stream completed");
    assert!(replied);
    let result = events(&response);
    let first = &result[0];
    let last = result.last().expect("answer");
    assert_eq!(last["raw"], "test answer");
    let history = json!([
        {"id": first["user_msg"]["id"], "role": "user", "content": first["user_msg"]["raw"]},
        {"id": last["id"], "role": "assistant", "content": last["raw"]},
    ]);
    let followup = client
        .post(format!("{base}/api/stream"))
        .json(&json!({
            "temporary": history, "action": {"type": "send", "content": "follow up"},
        }))
        .send()
        .await
        .expect("follow-up")
        .text()
        .await
        .expect("body");
    assert_eq!(
        events(&followup).last().expect("done")["raw"],
        "test answer"
    );
    for req in [
        json!({"temporary": [], "chat_id": "fake", "action":{"type":"send", "content":"x"}}),
        json!({"temporary": [], "action":{"type":"send", "attachments":["fake.txt"]}}),
        json!({"temporary": [{"id":"system", "role":"system", "content":"x"}], "action":{"type":"send", "content":"x"}}),
    ] {
        assert_eq!(
            client
                .post(format!("{base}/api/stream"))
                .json(&req)
                .send()
                .await
                .expect("send")
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert!(!dir.join("users/local/chats").exists());
    assert!(!dir.join("users/local/state.json").exists());
    assert!(
        state
            .storage()
            .list_sidebar("local")
            .await
            .expect("sidebar")
            .chats
            .is_empty()
    );
    assert!(
        state
            .scheduler()
            .list("local")
            .await
            .expect("schedule list")
            .is_empty()
    );
    app.abort();
    provider.abort();
    tokio::fs::remove_dir_all(dir).await.expect("cleanup");
}

#[tokio::test]
async fn schedules_resume_after_restart_and_run_without_tools() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let upstream = Router::new().route("/responses", post(|Json(body): Json<Value>| async move {
        assert!(body["tools"].is_null());
        assert_eq!(body["input"][0]["content"], "Daily question");
        Json(json!({"output": [{"type":"message", "content":[{"type":"output_text", "text":"Scheduled answer"}]}], "usage": {"total_tokens": 5}}))
    }));
    let (upstream, provider) = serve(upstream).await;
    let dir = PathBuf::from(format!("/tmp/opencode/sun-schedule-{}", new_id()));
    let raw = format!(
        r#"
[server]
data_dir = "{}"
[llm]
base_url = "{upstream}"
[[llm.models]]
id = "test-model"
"#,
        dir.display()
    );
    let make_state = || {
        AppState::new(
            dir.join("config.toml"),
            Config::parse_toml(&raw).expect("config"),
            TEST_SECRETS,
        )
        .expect("state")
    };
    let state = make_state();
    let (base, app) = serve(router(state.clone())).await;
    let client = reqwest::Client::new();
    let home = client
        .get(&base)
        .send()
        .await
        .expect("home")
        .text()
        .await
        .expect("HTML");
    assert!(home.contains("<summary>Automations</summary>"));
    assert!(home.contains("<form id=\"schedule-form\" class=\"runtime-group\">"));
    assert!(
        !home.contains("data-tool=\"schedule_prompt\"")
            && !home.contains("data-tool=\"fork_chat\"")
    );
    let run_at = Utc::now() + TimeDelta::seconds(1);
    for interval in [None, Some(5)] {
        let response = client
            .post(format!("{base}/api/schedules"))
            .json(&json!({
                "prompt": "Daily question", "run_at": run_at, "every_minutes": interval,
            }))
            .send()
            .await
            .expect("schedule");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    let jobs = state.scheduler().list("local").await.expect("jobs");
    assert!(
        state
            .scheduler()
            .list("other")
            .await
            .expect("other")
            .is_empty()
    );
    assert!(
        state
            .scheduler()
            .delete("other", &jobs[0].id)
            .await
            .is_err()
    );
    app.abort();
    drop(state);

    let restarted = make_state();
    let runner = tokio::spawn(Scheduler::run(restarted.clone()));
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let chats = restarted
                .storage()
                .list_sidebar("local")
                .await
                .expect("chats")
                .chats;
            if chats.len() == 2 {
                for summary in chats {
                    let chat = restarted
                        .storage()
                        .get_chat("local", &summary.id)
                        .await
                        .expect("chat");
                    assert_eq!(chat.messages[1].content, "Scheduled answer");
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both runs completed");
    let jobs = restarted
        .scheduler()
        .list("local")
        .await
        .expect("persisted jobs");
    assert_eq!(jobs[0].next_run, None);
    assert_eq!(jobs[1].next_run, Some(run_at + TimeDelta::minutes(5)));
    runner.abort();
    provider.abort();
    tokio::fs::remove_dir_all(dir).await.expect("cleanup");
}
