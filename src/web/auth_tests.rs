use super::router;
use crate::{
    auth::{ExtraClaims, derive_user_id},
    config::{Config, Secrets},
    domain::UserContext,
    state::AppState,
    storage::{Storage, new_id},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use openidconnect::{
    Audience, IdToken, IdTokenClaims, IssuerUrl, Nonce, StandardClaims, SubjectIdentifier,
    core::{
        CoreGenderClaim, CoreHmacKey, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm,
    },
    url::{Url, form_urlencoded},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::net::TcpListener;

const CLIENT_SECRET: &str = "test-only-confidential-client-secret";

struct Grant {
    nonce: String,
    challenge: String,
    subject: String,
    fault: String,
}

#[derive(Clone)]
struct Provider {
    issuer: String,
    grants: Arc<Mutex<BTreeMap<String, Grant>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn token(State(provider): State<Provider>, body: Bytes) -> Response {
    let form: BTreeMap<String, String> = form_urlencoded::parse(&body).into_owned().collect();
    let grant = provider
        .grants
        .lock()
        .expect("grants")
        .remove(form.get("code").map(String::as_str).unwrap_or(""));
    let Some(grant) = grant else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let verifier = form
        .get("code_verifier")
        .expect("PKCE verifier is required");
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)),
        grant.challenge
    );
    assert_eq!(form["grant_type"], "authorization_code");
    let now = Utc::now();
    let expires = now + Duration::seconds(if grant.fault == "expired" { -600 } else { 300 });
    let audience = if grant.fault == "audience" {
        "wrong-client"
    } else {
        "sun-test"
    };
    let issuer = if grant.fault == "issuer" {
        "https://wrong-issuer.example"
    } else {
        &provider.issuer
    };
    let nonce = if grant.fault == "nonce" {
        "wrong-nonce"
    } else {
        &grant.nonce
    };
    let values = if grant.subject == "alice" {
        BTreeMap::from([("groups".to_owned(), json!(["sre"]))])
    } else {
        BTreeMap::new()
    };
    let claims = IdTokenClaims::new(
        IssuerUrl::new(issuer.to_owned()).expect("issuer"),
        vec![Audience::new(audience.to_owned())],
        expires,
        now,
        StandardClaims::<CoreGenderClaim>::new(SubjectIdentifier::new(grant.subject.clone())),
        ExtraClaims { values },
    )
    .set_nonce(Some(Nonce::new(nonce.to_owned())));
    let secret = if grant.fault == "signature" {
        "wrong-signing-key"
    } else {
        CLIENT_SECRET
    };
    let token: IdToken<
        ExtraClaims,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    > = IdToken::new(
        claims,
        &CoreHmacKey::new(secret.as_bytes()),
        CoreJwsSigningAlgorithm::HmacSha256,
        None,
        None,
    )
    .expect("sign token");
    Json(json!({
        "access_token": format!("access-{}", grant.subject),
        "token_type": "Bearer", "expires_in": 300, "id_token": token.to_string(),
    }))
    .into_response()
}

async fn provider() -> (Provider, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("provider listener");
    let provider = Provider {
        issuer: format!(
            "http://{}",
            listener.local_addr().expect("provider address")
        ),
        grants: Arc::default(),
        requests: Arc::default(),
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(|State(p): State<Provider>| async move {
            Json(json!({
                "issuer": p.issuer,
                "authorization_endpoint": format!("{}/authorize", p.issuer),
                "token_endpoint": format!("{}/token", p.issuer),
                "userinfo_endpoint": format!("{}/userinfo", p.issuer),
                "jwks_uri": format!("{}/jwks", p.issuer),
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256", "HS256"],
                "code_challenge_methods_supported": ["S256"],
            }))
        }))
        .route("/jwks", get(|| async { Json(json!({"keys": []})) }))
        .route("/token", post(token))
        .route("/userinfo", get(|headers: HeaderMap| async move {
            let access = headers.get(header::AUTHORIZATION).expect("userinfo authorization").to_str().expect("header");
            let subject = access.strip_prefix("Bearer access-").expect("access token");
            Json(json!({"sub": if subject == "mismatch" { "someone-else" } else { subject }, "groups": ["viewer"]}))
        }))
        .route("/v1/responses", post(|State(p): State<Provider>, Json(body): Json<Value>| async move {
            p.requests.lock().expect("requests").push(body);
            Json(json!({
                "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Private answer"}]}],
                "usage": {"input_tokens": 90, "output_tokens": 20, "total_tokens": 110},
            }))
        })).with_state(provider.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("provider server");
    });
    (provider, server)
}

struct Login {
    url: Url,
    cookie: String,
}

async fn begin(
    client: &reqwest::Client,
    base: &str,
    provider: &Provider,
    subject: &str,
    fault: &str,
) -> Login {
    let start = client
        .get(format!("{base}/auth/login"))
        .send()
        .await
        .expect("start login");
    assert_eq!(start.status(), StatusCode::SEE_OTHER);
    let authorize = Url::parse(
        start.headers()[header::LOCATION]
            .to_str()
            .expect("location"),
    )
    .expect("authorize URL");
    let params: BTreeMap<String, String> = authorize.query_pairs().into_owned().collect();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(params["scope"].split(' ').any(|s| s == "openid"));
    let cookie = start.headers()[header::SET_COOKIE]
        .to_str()
        .expect("state cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();
    let code = new_id();
    provider.grants.lock().expect("grants").insert(
        code.clone(),
        Grant {
            nonce: params["nonce"].clone(),
            challenge: params["code_challenge"].clone(),
            subject: subject.to_owned(),
            fault: fault.to_owned(),
        },
    );
    let mut url = Url::parse(&format!("{base}/auth/callback")).expect("callback URL");
    url.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"]);
    Login { url, cookie }
}

async fn complete(client: &reqwest::Client, login: &Login) -> reqwest::Response {
    client
        .get(login.url.clone())
        .header(header::COOKIE, &login.cookie)
        .send()
        .await
        .expect("callback")
}

async fn sign_in(
    client: &reqwest::Client,
    base: &str,
    provider: &Provider,
    subject: &str,
) -> String {
    let flow = begin(client, base, provider, subject, "").await;
    let response = complete(client, &flow).await;
    let status = response.status();
    if status != StatusCode::SEE_OTHER {
        panic!(
            "login failed: {status}: {}",
            response.text().await.expect("body")
        );
    }
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let cookies: Vec<&str> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|h| h.to_str().expect("cookie"))
        .collect();
    let session = cookies
        .iter()
        .find(|c| c.starts_with("sun_session="))
        .expect("session");
    assert!(session.contains("HttpOnly") && session.contains("SameSite=Lax"));
    assert!(!session.contains("alice") && !session.contains("sre"));
    session.split(';').next().expect("cookie pair").to_owned()
}

#[tokio::test]
async fn oidc_login_validation_group_policies_isolation_and_quota() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (provider, provider_server) = provider().await;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("app listener");
    let base = format!("http://{}", listener.local_addr().expect("app address"));
    let dir = PathBuf::from(format!("/tmp/opencode/sun-oidc-{}", new_id()));
    tokio::fs::create_dir_all(&dir).await.expect("data dir");
    let cfg_path = dir.join("config.toml");
    let config = format!(
        r#"
[server]
data_dir = "{}/data"
[auth]
enabled = true
issuer_url = "{}"
client_id = "sun-test"
redirect_url = "{base}/auth/callback"
[files]
max_file_size = 8
[llm]
base_url = "{}/v1"
default_model = "small"
max_m_tokens = 0.00005
[[llm.models]]
id = "small"
[[llm.models]]
id = "big"
[[mcp]]
name = "github"
url = "http://127.0.0.1:9/mcp"
[groups.viewer]
allowed_models = ["small"]
allowed_mcp = []
allowed_tools = ["current_timestamp", "shared", "ask_user", "fork_chat"]
[groups.sre]
allowed_models = ["big"]
allowed_mcp = ["github"]
max_file_size = 1024
max_m_tokens = 0.0001
[groups.sre.tools]
search = true
[groups.sre.prompts]
system_prompt = "SRE_ONLY_PROMPT"
"#,
        dir.display(),
        provider.issuer,
        provider.issuer
    );
    tokio::fs::write(&cfg_path, &config).await.expect("config");
    let secrets = Secrets {
        llm_api_key: None,
        oidc_client_secret: Some(CLIENT_SECRET.to_owned()),
    };
    let state = AppState::new(
        cfg_path.clone(),
        Config::parse_toml(&config).expect("parse"),
        secrets,
    )
    .expect("state");
    state.spawn_config_watcher();
    let app_state = state.clone();
    let app_server = tokio::spawn(async move {
        axum::serve(listener, router(app_state))
            .await
            .expect("app server");
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    assert_eq!(
        client.get(&base).send().await.expect("anonymous").status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        client
            .post(format!("{base}/api/chats"))
            .json(&json!({}))
            .send()
            .await
            .expect("anonymous API")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&base)
            .header("x-forwarded-user", "alice")
            .send()
            .await
            .expect("spoofed identity")
            .status(),
        StatusCode::SEE_OTHER
    );

    let bad_state = begin(&client, &base, &provider, "alice", "").await;
    assert_eq!(
        client
            .get(bad_state.url.clone())
            .send()
            .await
            .expect("no cookie")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(bad_state.url.clone())
            .header(header::COOKIE, "sun_oidc_state=wrong")
            .send()
            .await
            .expect("bad state")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for fault in ["nonce", "signature", "audience", "issuer", "expired"] {
        let flow = begin(&client, &base, &provider, "alice", fault).await;
        assert_eq!(
            complete(&client, &flow).await.status(),
            StatusCode::UNAUTHORIZED,
            "{fault}"
        );
    }
    let mismatch = begin(&client, &base, &provider, "mismatch", "").await;
    assert_eq!(
        complete(&client, &mismatch).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let alice = sign_in(&client, &base, &provider, "alice").await;
    let bob = sign_in(&client, &base, &provider, "bob").await;
    let replay = begin(&client, &base, &provider, "bob", "").await;
    assert_eq!(
        complete(&client, &replay).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        complete(&client, &replay).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let alice_uid = derive_user_id(&provider.issuer, "alice");
    let bob_uid = derive_user_id(&provider.issuer, "bob");
    assert_ne!(alice_uid, bob_uid);
    assert_ne!(derive_user_id("https://another-issuer", "alice"), alice_uid);
    let alice_page = client
        .get(&base)
        .header(header::COOKIE, &alice)
        .send()
        .await
        .expect("alice page")
        .text()
        .await
        .expect("HTML");
    let bob_page = client
        .get(&base)
        .header(header::COOKIE, &bob)
        .send()
        .await
        .expect("bob page")
        .text()
        .await
        .expect("HTML");
    assert!(alice_page.contains("value=\"big\"") && !alice_page.contains("value=\"small\""));
    assert!(bob_page.contains("value=\"small\"") && !bob_page.contains("value=\"big\""));
    assert!(!bob_page.contains("data-mcp=\"github\""));
    assert!(!bob_page.contains("data-tool=\"search\""));
    assert!(alice_page.contains("data-tool=\"search\" checked"));
    for (cookie, tool, status) in [
        (&bob, "search", StatusCode::NOT_FOUND),
        (&alice, "shared", StatusCode::NO_CONTENT),
    ] {
        assert_eq!(
            client
                .post(format!("{base}/api/tools"))
                .header(header::COOKIE, cookie)
                .json(&json!({"name": tool, "enabled": tool == "search"}))
                .send()
                .await
                .expect("save tool preference")
                .status(),
            status
        );
    }

    let alice_chat: Value = client
        .post(format!("{base}/api/chats"))
        .header(header::COOKIE, &alice)
        .json(&json!({}))
        .send()
        .await
        .expect("alice create")
        .json()
        .await
        .expect("chat");
    let cid = alice_chat["id"].as_str().expect("chat ID");
    assert_eq!(alice_chat["model"], "big");
    assert_eq!(
        client
            .post(format!("{base}/api/chats/{cid}/fork"))
            .header(header::COOKIE, &bob)
            .json(&json!({"through": "sample-message"}))
            .send()
            .await
            .expect("cross-user fork")
            .status(),
        StatusCode::NOT_FOUND
    );
    let bob_chat: Value = client
        .post(format!("{base}/api/chats"))
        .header(header::COOKIE, &bob)
        .json(&json!({}))
        .send()
        .await
        .expect("bob create")
        .json()
        .await
        .expect("chat");
    assert_eq!(bob_chat["model"], "small");
    assert_eq!(
        client
            .post(format!("{base}/api/preferences"))
            .header(header::COOKIE, &bob)
            .json(&json!({"model":"big"}))
            .send()
            .await
            .expect("denied model")
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .post(format!("{base}/api/mcp"))
            .header(header::COOKIE, &bob)
            .json(&json!({"name":"github", "enabled":true, "auth_key":"bob-secret"}))
            .send()
            .await
            .expect("denied MCP")
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(client.post(format!("{base}/api/stream")).header(header::COOKIE, &bob).json(&json!({"model":"small", "enabled_mcp":["github"], "action":{"type":"send","content":"blocked"}})).send().await.expect("denied direct MCP").status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        client
            .post(format!("{base}/api/mcp"))
            .header(header::COOKIE, &alice)
            .json(&json!({"name":"github", "enabled":false, "auth_key":"alice-secret"}))
            .send()
            .await
            .expect("MCP state")
            .status(),
        StatusCode::NO_CONTENT
    );

    let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"private.txt\"\r\nContent-Type: text/plain\r\n\r\nALICE_PRIVATE_DATA\r\n--boundary--\r\n";
    for (cookie, chat, expected) in [
        (&alice, cid, StatusCode::OK),
        (
            &bob,
            bob_chat["id"].as_str().expect("bob ID"),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        assert_eq!(
            client
                .post(format!("{base}/api/chats/{chat}/files"))
                .header(header::COOKIE, cookie)
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=boundary"
                )
                .body(body)
                .send()
                .await
                .expect("upload")
                .status(),
            expected
        );
    }
    for path in [
        format!("/c/{cid}"),
        format!("/api/chats/{cid}/files"),
        format!("/api/chats/{cid}/files/private.txt"),
    ] {
        assert_eq!(
            client
                .get(format!("{base}{path}"))
                .header(header::COOKIE, &bob)
                .send()
                .await
                .expect("cross-user read")
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        client
            .delete(format!("{base}/api/chats/{cid}"))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .expect("cross-user delete")
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .patch(format!("{base}/api/chats/{cid}"))
            .header(header::COOKIE, &bob)
            .json(&json!({"title":"stolen"}))
            .send()
            .await
            .expect("cross-user patch")
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .post(format!("{base}/api/chats/{cid}/compact"))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .expect("cross-user compact")
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .post(format!("{base}/api/preferences"))
            .header(header::COOKIE, &alice)
            .header(header::ORIGIN, "https://evil.example")
            .json(&json!({"model":"big"}))
            .send()
            .await
            .expect("CSRF")
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let (qid, reply) = state.questions().register(&alice_uid, cid).await;
    assert_eq!(
        client
            .post(format!("{base}/api/chats/{cid}/answer"))
            .header(header::COOKIE, &bob)
            .json(&json!({"question_id":qid,"answer":"injected"}))
            .send()
            .await
            .expect("cross-user answer")
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .post(format!("{base}/api/chats/{cid}/answer"))
            .header(header::COOKIE, &alice)
            .json(&json!({"question_id":qid,"answer":"valid"}))
            .send()
            .await
            .expect("answer")
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(reply.await.expect("reply"), "valid");

    let stream = client.post(format!("{base}/api/stream")).header(header::COOKIE, &alice).json(&json!({"chat_id":cid,"model":"big","action":{"type":"send","content":"Alice private question"}})).send().await.expect("stream").text().await.expect("stream text");
    assert!(stream.contains("Private answer"), "{stream}");
    assert_eq!(client.post(format!("{base}/api/stream")).header(header::COOKIE, &alice).json(&json!({"chat_id":cid,"model":"big","action":{"type":"send","content":"over quota"}})).send().await.expect("quota").status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        client
            .post(format!("{base}/api/chats/{cid}/compact"))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .expect("compaction quota")
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let restarted = Storage::new(dir.join("data"));
    assert_eq!(
        restarted
            .get_user_state(&alice_uid)
            .await
            .expect("alice state")
            .tools
            .get("shared"),
        Some(&false),
    );
    assert!(
        restarted
            .get_user_state(&bob_uid)
            .await
            .expect("bob state")
            .tools
            .is_empty()
    );
    assert!(
        restarted
            .check_token_limit(&alice_uid, Some(100))
            .await
            .is_err()
    );
    assert!(
        restarted
            .check_token_limit(&bob_uid, Some(50))
            .await
            .is_ok()
    );
    assert!(
        state
            .storage()
            .search_other_chats(
                &bob_uid,
                bob_chat["id"].as_str().expect("bob ID"),
                "ALICE_PRIVATE_DATA"
            )
            .await
            .expect("search")
            .is_empty()
    );
    assert!(
        state
            .storage()
            .get_user_state(&bob_uid)
            .await
            .expect("bob state")
            .mcp
            .is_empty()
    );
    assert_eq!(
        state
            .storage()
            .get_user_state(&alice_uid)
            .await
            .expect("alice state")
            .mcp["github"]
            .auth_key
            .as_deref(),
        Some("alice-secret")
    );
    {
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["instructions"], "SRE_ONLY_PROMPT");
        assert!(
            requests[0]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .any(|t| t["name"] == "search")
        );
    }

    assert_eq!(
        client
            .post(format!("{base}/auth/logout"))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .expect("logout")
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        client
            .get(&base)
            .header(header::COOKIE, &alice)
            .send()
            .await
            .expect("replay old session")
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        client
            .get(&base)
            .header(header::COOKIE, &bob)
            .send()
            .await
            .expect("other session unaffected")
            .status(),
        StatusCode::OK
    );

    let mut cfg = Config::parse_toml(&config).expect("cfg");
    cfg.auth.client_id = Some("other-client".to_owned());
    let headers = HeaderMap::from_iter([(header::COOKIE, bob.parse().expect("cookie"))]);
    assert!(
        state
            .auth()
            .verify_session(&cfg.auth, &headers)
            .await
            .is_none()
    );
    assert!(state.effective_for(&UserContext::local()).models.len() == 2);

    let remaining = sign_in(&client, &base, &provider, "alice").await;
    let reconfigured = config.replace("client_id = \"sun-test\"", "client_id = \"other-client\"");
    tokio::fs::write(&cfg_path, reconfigured)
        .await
        .expect("update OIDC config");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if state.config().auth.client_id.as_deref() == Some("other-client") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("config reload");
    assert_eq!(
        client
            .get(&base)
            .header(header::COOKIE, &remaining)
            .send()
            .await
            .expect("invalidated session")
            .status(),
        StatusCode::SEE_OTHER
    );

    app_server.abort();
    provider_server.abort();
    tokio::fs::remove_dir_all(dir).await.expect("cleanup");
}
