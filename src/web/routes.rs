use crate::{
    config::EffectiveConfig,
    domain::{Attachment, BuiltinTool, Chat, Message, Role, SharedFile, UserContext},
    error::AppError,
    files,
    llm::{ChatCompletionRequest, LlmEvent, PreparedMessage},
    markdown,
    mcp::clean_bearer,
    schedule::ScheduleInput,
    state::AppState,
    storage::{new_id, now_millis, sanitize_folder},
    tools,
    web::views::{
        McpViewItem, PageView, render_page, render_schedules, render_tool_activity, tool_preview,
    },
};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Multipart, Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::stream;
use maud::Markup;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn index(State(state): State<AppState>, user: UserContext) -> Result<Markup, AppError> {
    render_chat_view(&state, &user, None).await
}

pub async fn chat_page(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
) -> Result<Markup, AppError> {
    let chat = state.storage().get_chat(&user.user_id, &id).await?;
    render_chat_view(&state, &user, Some(chat)).await
}

async fn render_chat_view(
    state: &AppState,
    user: &UserContext,
    chat: Option<Chat>,
) -> Result<Markup, AppError> {
    let cfg = state.config();
    let effective = state.effective_for(user);
    let sidebar = state.storage().list_sidebar(&user.user_id).await?;
    let user_state = state.storage().get_user_state(&user.user_id).await?;
    let models = &effective.models;
    let shared_files = match &chat {
        Some(c) => files::list_shared_files(state.storage(), &user.user_id, &c.id).await?,
        None => Vec::new(),
    };
    let active_mcp = effective.resolve_enabled_mcp(
        &user_state,
        chat.as_ref().and_then(|c| c.enabled_mcp.as_deref()),
    );
    let mcp_views: Vec<McpViewItem> = effective
        .mcp
        .iter()
        .map(|s| McpViewItem {
            name: s.name.clone(),
            url: s.url.clone(),
            enabled: active_mcp.iter().any(|a| a.name == s.name),
            has_key: user_state
                .mcp
                .get(&s.name)
                .and_then(|u| clean_bearer(u.auth_key.as_deref()))
                .is_some(),
        })
        .collect();
    let builtin_tools: Vec<_> = effective.builtin_tools(&user_state).collect();

    let (chosen, reasoning) = state.resolve_selection(
        &effective,
        chat.as_ref().map(|c| c.model.as_str()),
        chat.as_ref().and_then(|c| c.reasoning.as_deref()),
        &user_state,
        false,
    )?;

    Ok(render_page(&PageView {
        config: &cfg,
        effective: &effective,
        sidebar: &sidebar,
        chat: chat.as_ref(),
        shared_files: &shared_files,
        builtin_tools: &builtin_tools,
        mcp_views: &mcp_views,
        models,
        active_model: chosen.as_ref().map(|m| m.id.as_str()),
        active_reasoning: reasoning.as_deref(),
    }))
}

#[derive(Deserialize)]
pub struct PreferencesPayload {
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

pub async fn update_preferences(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<PreferencesPayload>,
) -> Result<StatusCode, AppError> {
    let effective = state.effective_for(&user);
    let user_state = state.storage().get_user_state(&user.user_id).await?;
    let (chosen, reasoning) = state.resolve_selection(
        &effective,
        payload.model.as_deref(),
        payload.reasoning.as_deref(),
        &user_state,
        true,
    )?;
    state
        .storage()
        .update_preferences(&user.user_id, chosen.map(|m| m.id), Some(reasoning))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UpdateToolPayload {
    name: BuiltinTool,
    enabled: bool,
}

pub async fn update_tool(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<UpdateToolPayload>,
) -> Result<StatusCode, AppError> {
    if payload.name.label().is_none()
        || !state
            .effective_for(&user)
            .allowed_tools
            .contains(&payload.name)
    {
        return Err(AppError::NotFound(format!(
            "tool '{}' is not allowed",
            payload.name.name()
        )));
    }
    state
        .storage()
        .update_tool_state(&user.user_id, payload.name, payload.enabled)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_schedules(
    State(state): State<AppState>,
    user: UserContext,
) -> Result<Markup, AppError> {
    Ok(render_schedules(
        &state.scheduler().list(&user.user_id).await?,
    ))
}

#[derive(Deserialize)]
pub struct SchedulePayload {
    #[serde(flatten)]
    schedule: ScheduleInput,
    model: Option<String>,
    reasoning: Option<String>,
}

pub async fn add_schedule(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<SchedulePayload>,
) -> Result<StatusCode, AppError> {
    let cfg = state.effective_for(&user);
    let prefs = state.storage().get_user_state(&user.user_id).await?;
    if !cfg.allowed_tools.contains(&BuiltinTool::SchedulePrompt) {
        return Err(AppError::NotFound("automations are not allowed".to_owned()));
    }
    let (model, reasoning) = state.resolve_selection(
        &cfg,
        payload.model.as_deref(),
        payload.reasoning.as_deref(),
        &prefs,
        true,
    )?;
    let model = model.ok_or_else(|| AppError::BadRequest("no models configured".to_owned()))?;
    state
        .scheduler()
        .add(&user.user_id, &model.id, reasoning, payload.schedule)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_schedule(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    state.scheduler().delete(&user.user_id, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UpdateMcpPayload {
    pub name: String,
    pub enabled: Option<bool>,
    pub auth_key: Option<String>,
}

pub async fn update_mcp(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<UpdateMcpPayload>,
) -> Result<StatusCode, AppError> {
    let cfg = state.effective_for(&user);
    let srv = cfg
        .mcp
        .iter()
        .find(|s| s.name == payload.name)
        .ok_or_else(|| AppError::NotFound(format!("mcp server '{}' not found", payload.name)))?;

    let auth_update = payload
        .auth_key
        .as_deref()
        .map(|raw| clean_bearer(Some(raw)));
    let evict = auth_update.is_some() || payload.enabled == Some(false);
    state
        .storage()
        .update_mcp_state(&user.user_id, &srv.name, payload.enabled, auth_update)
        .await?;

    if evict {
        state.mcp().evict_server(&user.user_id, &srv.name).await;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct CreateChatPayload {
    pub folder: Option<String>,
    pub enabled_mcp: Option<Vec<String>>,
}

pub async fn create_chat(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<CreateChatPayload>,
) -> Result<Json<Chat>, AppError> {
    let cfg = state.effective_for(&user);
    let user_state = state.storage().get_user_state(&user.user_id).await?;
    let (chosen, reasoning) = state.resolve_selection(&cfg, None, None, &user_state, false)?;
    let model_id = chosen
        .map(|m| m.id)
        .ok_or_else(|| AppError::BadRequest("no models configured".to_owned()))?;
    let enabled_mcp = filter_allowed_mcp(&cfg, payload.enabled_mcp)?;
    Ok(Json(
        state
            .storage()
            .create_chat(
                &user.user_id,
                model_id,
                reasoning,
                payload.folder,
                enabled_mcp,
            )
            .await?,
    ))
}

#[derive(Deserialize)]
pub struct PatchChatPayload {
    pub title: Option<String>,
    pub folder: Option<String>,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub enabled_mcp: Option<Vec<String>>,
}

pub async fn patch_chat(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
    Json(payload): Json<PatchChatPayload>,
) -> Result<Json<Chat>, AppError> {
    let _guard = state.generation_guard(&user.user_id).await?;
    let cfg = state.effective_for(&user);
    let mut chat = state.storage().get_chat(&user.user_id, &id).await?;
    let user_state = state.storage().get_user_state(&user.user_id).await?;

    if let Some(title) = payload
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        chat.title = title.chars().take(80).collect();
    }
    if let Some(folder) = payload.folder {
        let trimmed = folder.trim();
        chat.folder = (!trimmed.is_empty())
            .then(|| sanitize_folder(trimmed))
            .transpose()?;
    }
    if let Some(sys) = payload.system_prompt {
        let trimmed = sys.trim();
        chat.system_prompt = (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }
    if payload.enabled_mcp.is_some() {
        chat.enabled_mcp = filter_allowed_mcp(&cfg, payload.enabled_mcp)?;
    }
    if payload.model.is_some() || payload.reasoning.is_some() {
        let req_model = payload.model.as_deref().or(Some(chat.model.as_str()));
        let req_reasoning = match &payload.reasoning {
            Some(r) => Some(r.as_str()),
            None if payload.model.is_none() => chat.reasoning.as_deref(),
            None => None,
        };
        let (chosen, reasoning) =
            state.resolve_selection(&cfg, req_model, req_reasoning, &user_state, true)?;
        if let Some(m) = chosen {
            chat.model.clone_from(&m.id);
            chat.reasoning.clone_from(&reasoning);
            state
                .storage()
                .update_preferences(&user.user_id, Some(m.id), Some(reasoning))
                .await?;
        }
    }

    chat.updated_at = now_millis();
    state.storage().save_chat(&user.user_id, &chat).await?;
    Ok(Json(chat))
}

pub async fn delete_chat(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let _guard = state.generation_guard(&user.user_id).await?;
    state.storage().delete_chat(&user.user_id, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct ForkPayload {
    through: String,
}

pub async fn fork_chat(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
    Json(payload): Json<ForkPayload>,
) -> Result<Json<crate::domain::ChatSummary>, AppError> {
    let _guard = state.generation_guard(&user.user_id).await?;
    if !state
        .effective_for(&user)
        .allowed_tools
        .contains(&BuiltinTool::ForkChat)
    {
        return Err(AppError::NotFound("forking is not allowed".to_owned()));
    }
    let store = state.storage();
    let chat = store.get_chat(&user.user_id, &id).await?;
    Ok(Json(
        store
            .fork_chat(&user.user_id, chat, &payload.through)
            .await?,
    ))
}

pub async fn list_files(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
) -> Result<Json<Vec<SharedFile>>, AppError> {
    let _ = state.storage().get_chat(&user.user_id, &id).await?;
    let items = files::list_shared_files(state.storage(), &user.user_id, &id).await?;
    Ok(Json(items))
}

pub async fn upload_files(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
    multipart: Multipart,
) -> Result<Json<Vec<SharedFile>>, AppError> {
    let _guard = state.generation_guard(&user.user_id).await?;
    let max_size = state.effective_for(&user).max_file_size;
    let uploaded =
        files::upload_multipart(state.storage(), &user.user_id, &id, max_size, multipart).await?;
    Ok(Json(uploaded))
}

pub async fn download_file(
    State(state): State<AppState>,
    user: UserContext,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let (safe_name, mime, bytes) =
        files::read_shared_file(state.storage(), &user.user_id, &id, &name).await?;
    let disp_kind = if mime.starts_with("image/") {
        "inline"
    } else {
        "attachment"
    };
    let escaped_name: String = safe_name
        .chars()
        .filter(|&c| c != '"' && c != '\\' && !c.is_control())
        .collect();
    let disposition = format!("{disp_kind}; filename=\"{escaped_name}\"");

    let mut resp = Bytes::from(bytes).into_response();
    let headers = resp.headers_mut();
    if let Ok(ct) = HeaderValue::from_str(&mime) {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    if let Ok(cd) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, cd);
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox",
        ),
    );
    Ok(resp)
}

pub async fn delete_file(
    State(state): State<AppState>,
    user: UserContext,
    Path((id, name)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let _guard = state.generation_guard(&user.user_id).await?;
    files::delete_shared_file(state.storage(), &user.user_id, &id, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct AnswerQuestionPayload {
    pub question_id: String,
    pub answer: String,
}

pub async fn answer_question(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
    Json(payload): Json<AnswerQuestionPayload>,
) -> Result<StatusCode, AppError> {
    let trimmed = payload.answer.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("answer must not be empty".to_owned()));
    }
    state
        .questions()
        .answer(&user.user_id, &id, &payload.question_id, trimmed.to_owned())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn compact_chat(
    State(state): State<AppState>,
    user: UserContext,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let _guard = state.generation_guard(&user.user_id).await?;
    let cfg = state.config();
    let effective = state.effective_for(&user);
    let mut chat = state.storage().get_chat(&user.user_id, &id).await?;
    if chat.messages.len() < 2 {
        return Err(AppError::BadRequest(
            "at least two messages are required to compact memory".to_owned(),
        ));
    }

    let user_state = state.storage().get_user_state(&user.user_id).await?;
    let (chosen_opt, reasoning) = state.resolve_selection(
        &effective,
        Some(&chat.model),
        chat.reasoning.as_deref(),
        &user_state,
        false,
    )?;
    let chosen_model =
        chosen_opt.ok_or_else(|| AppError::BadRequest("no models configured".to_owned()))?;

    let mut prepared =
        files::prepare_messages(state.storage(), &user.user_id, &chat.id, &chat.messages).await;
    prepared.push(PreparedMessage {
        role: Role::User,
        text: "Compact our entire conversation above into a dense, self-contained context summary preserving all key facts, technical details, code snippets, file names, decisions, and open tasks.".to_owned(),
        parts: Vec::new(),
        reasoning_items: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    });

    let mut llm_rx = state
        .stream_completion(&user, &effective, ChatCompletionRequest {
            base_url: &cfg.llm.base_url,
            api_key: state.secrets().llm_api_key.as_deref(),
            model: &chosen_model.id,
            reasoning: reasoning.as_deref(),
            system_prompt: Some(
                "You are a precision context-compaction engine. Output only a structured, dense Markdown summary of the conversation with zero filler.",
            ),
            messages: &prepared,
            tools: &[],
            timeout: None,
        })
        .await?;

    let mut summary = String::new();
    while let Some(ev) = llm_rx.recv().await {
        match ev? {
            LlmEvent::Delta { content, .. } => summary.push_str(&content),
            LlmEvent::ToolCalls { .. } => {}
            LlmEvent::Usage(_) => {}
        }
    }

    let summary_trimmed = summary.trim();
    if summary_trimmed.is_empty() {
        return Err(AppError::Upstream(
            "compaction returned an empty summary".to_owned(),
        ));
    }

    let now = now_millis();
    let user_marker = Message {
        id: new_id(),
        role: Role::User,
        content: "[Compacted conversation context]".to_owned(),
        model_name: None,
        reasoning: None,
        reasoning_items: Vec::new(),
        attachments: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        created_at: now,
    };
    let assistant_summary = make_assistant_msg(
        new_id(),
        chosen_model.display_name(),
        summary_trimmed.to_owned(),
        String::new(),
    );

    let resp_json = json!({
        "user_msg": {
            "id": user_marker.id,
            "html": markdown::render(&user_marker.content),
            "raw": user_marker.content,
        },
        "assistant_msg": {
            "id": assistant_summary.id,
            "html": markdown::render(&assistant_summary.content),
            "raw": assistant_summary.content,
            "model_name": assistant_summary.model_name,
        }
    });

    chat.messages = vec![user_marker, assistant_summary];
    chat.updated_at = now_millis();
    state.storage().save_chat(&user.user_id, &chat).await?;

    Ok(Json(resp_json))
}

#[derive(Deserialize)]
pub struct FolderNamePayload {
    pub name: String,
}

#[derive(Deserialize)]
pub struct RenameFolderPayload {
    pub old_name: String,
    pub new_name: String,
}

pub async fn create_folder(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<FolderNamePayload>,
) -> Result<StatusCode, AppError> {
    state
        .storage()
        .create_folder(&user.user_id, &payload.name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rename_folder(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<RenameFolderPayload>,
) -> Result<StatusCode, AppError> {
    state
        .storage()
        .rename_folder(&user.user_id, &payload.old_name, &payload.new_name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_folder(
    State(state): State<AppState>,
    user: UserContext,
    Json(payload): Json<FolderNamePayload>,
) -> Result<StatusCode, AppError> {
    state
        .storage()
        .delete_folder(&user.user_id, &payload.name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamAction {
    Send {
        #[serde(default)]
        content: String,
        #[serde(default)]
        attachments: Vec<String>,
    },
    Edit {
        message_id: String,
        content: String,
    },
    Retry {
        message_id: String,
    },
}

#[derive(Deserialize)]
pub struct StreamRequest {
    pub chat_id: Option<String>,
    pub folder: Option<String>,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub enabled_mcp: Option<Vec<String>>,
    temporary: Option<Vec<TemporaryMessage>>,
    browser_time_zone: Option<tools::BrowserTimeZone>,
    pub action: StreamAction,
}

#[derive(Deserialize)]
struct TemporaryMessage {
    id: String,
    role: Role,
    content: String,
}

pub async fn stream_chat(
    State(state): State<AppState>,
    user: UserContext,
    Json(req): Json<StreamRequest>,
) -> Result<Response, AppError> {
    let guard = state.generation_guard(&user.user_id).await?;
    let cfg = state.config();
    let effective = state.effective_for(&user);
    state
        .storage()
        .check_token_limit(&user.user_id, effective.max_tokens_limit())
        .await?;
    let prev_state = state.storage().get_user_state(&user.user_id).await?;
    let (chosen_opt, reasoning) = state.resolve_selection(
        &effective,
        req.model.as_deref(),
        req.reasoning.as_deref(),
        &prev_state,
        true,
    )?;
    let chosen_model =
        chosen_opt.ok_or_else(|| AppError::BadRequest("no models configured".to_owned()))?;

    let temporary = req.temporary.is_some();
    let user_state = if temporary {
        prev_state
    } else {
        state
            .storage()
            .update_preferences(
                &user.user_id,
                Some(chosen_model.id.clone()),
                Some(reasoning.clone()),
            )
            .await?
    };

    let mut chat = if let Some(history) = req.temporary {
        if req.chat_id.is_some()
            || matches!(&req.action, StreamAction::Send { attachments, .. } if !attachments.is_empty())
        {
            return Err(AppError::BadRequest(
                "temporary chats cannot use saved chats or attachments".to_owned(),
            ));
        }
        let mut chat = Chat::new(chosen_model.id.clone(), reasoning.clone());
        for entry in history {
            crate::storage::validate_id(&entry.id)?;
            if !matches!(entry.role, Role::User | Role::Assistant) {
                return Err(AppError::BadRequest(
                    "invalid temporary message role".to_owned(),
                ));
            }
            chat.messages.push(Message {
                id: entry.id,
                ..Message::text(entry.role, entry.content)
            });
        }
        chat
    } else {
        let req_mcp = filter_allowed_mcp(&effective, req.enabled_mcp)?;
        let mut chat = match req.chat_id.as_deref().filter(|s| !s.is_empty()) {
            Some(id) => state.storage().get_chat(&user.user_id, id).await?,
            None => {
                state
                    .storage()
                    .create_chat(
                        &user.user_id,
                        chosen_model.id.clone(),
                        reasoning.clone(),
                        req.folder,
                        None,
                    )
                    .await?
            }
        };
        if req_mcp.is_some() {
            chat.enabled_mcp = req_mcp;
        }
        chat
    };

    chat.model.clone_from(&chosen_model.id);
    chat.reasoning.clone_from(&reasoning);
    if let Some(sys) = req.system_prompt {
        let trimmed = sys.trim();
        chat.system_prompt = (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }

    let user_msg_event = apply_stream_action(&state, &user.user_id, &mut chat, req.action).await?;
    chat.updated_at = now_millis();
    if !temporary {
        state.storage().save_chat(&user.user_id, &chat).await?;
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(32);
    let mut assistant_id = new_id();

    let _ = send_ndjson(
        &tx,
        &json!({
            "type": "init",
            "chat_id": chat.id,
            "title": chat.title,
            "user_msg": user_msg_event,
            "assistant_id": assistant_id,
            "model_name": chosen_model.display_name(),
        }),
    )
    .await;

    tokio::spawn(async move {
        let _guard = guard;
        let model_name = chosen_model.display_name();
        let sys_prompt =
            effective.resolve_system_prompt(&chosen_model.id, chat.system_prompt.as_deref());
        let active_mcp = if temporary {
            Vec::new()
        } else {
            effective.resolve_enabled_mcp(&user_state, chat.enabled_mcp.as_deref())
        };
        let (mut tools, mcp_routes) = match state
            .mcp()
            .discover_tools(&user.user_id, &active_mcp, &user_state)
            .await
        {
            Ok(pair) => pair,
            Err(err) => {
                let _ =
                    send_ndjson(&tx, &json!({ "type": "error", "message": err.to_string() })).await;
                return;
            }
        };
        let active_tools: Vec<_> = effective
            .builtin_tools(&user_state)
            .filter_map(|(tool, enabled)| {
                (enabled
                    && (!temporary
                        || matches!(
                            tool,
                            BuiltinTool::CurrentTimestamp
                                | BuiltinTool::AskUser
                                | BuiltinTool::CurrentLocation
                        )))
                .then_some(tool)
            })
            .collect();
        tools.extend(
            active_tools
                .iter()
                .copied()
                .filter_map(tools::builtin_tool_spec),
        );

        let mut content = String::new();
        let mut thought = String::new();

        let saved_user = (!temporary).then_some(user.user_id.as_str());
        for _ in 0..12 {
            let prepared =
                files::prepare_messages(state.storage(), &user.user_id, &chat.id, &chat.messages)
                    .await;
            let mut llm_rx = match state
                .stream_completion(
                    &user,
                    &effective,
                    ChatCompletionRequest {
                        base_url: &cfg.llm.base_url,
                        api_key: state.secrets().llm_api_key.as_deref(),
                        model: &chosen_model.id,
                        reasoning: reasoning.as_deref(),
                        system_prompt: sys_prompt.as_deref(),
                        messages: &prepared,
                        tools: &tools,
                        timeout: None,
                    },
                )
                .await
            {
                Ok(r) => r,
                Err(err) => {
                    let _ =
                        send_ndjson(&tx, &json!({ "type": "error", "message": err.to_string() }))
                            .await;
                    return;
                }
            };

            let mut last_render = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
            let mut dirty = false;
            let mut pending_calls = Vec::new();
            let mut pending_reasoning_items = Vec::new();

            while let Some(ev) = llm_rx.recv().await {
                match ev {
                    Ok(LlmEvent::Delta {
                        content: c_delta,
                        reasoning: r_delta,
                    }) => {
                        content.push_str(&c_delta);
                        thought.push_str(&r_delta);
                        dirty = true;
                        if last_render.elapsed() >= Duration::from_millis(35) {
                            if !send_render(&tx, &content, &thought).await {
                                persist_assistant(
                                    &state,
                                    saved_user,
                                    &mut chat,
                                    &assistant_id,
                                    model_name,
                                    &content,
                                    &thought,
                                )
                                .await;
                                return;
                            }
                            last_render = Instant::now();
                            dirty = false;
                        }
                    }
                    Ok(LlmEvent::ToolCalls {
                        calls,
                        reasoning_items,
                    }) => {
                        pending_calls = calls;
                        pending_reasoning_items = reasoning_items;
                    }
                    Ok(LlmEvent::Usage(_)) => {}
                    Err(err) => {
                        persist_assistant(
                            &state,
                            saved_user,
                            &mut chat,
                            &assistant_id,
                            model_name,
                            &content,
                            &thought,
                        )
                        .await;
                        let _ = send_ndjson(
                            &tx,
                            &json!({ "type": "error", "message": err.to_string() }),
                        )
                        .await;
                        return;
                    }
                }
            }

            if dirty && !send_render(&tx, &content, &thought).await {
                persist_assistant(
                    &state,
                    saved_user,
                    &mut chat,
                    &assistant_id,
                    model_name,
                    &content,
                    &thought,
                )
                .await;
                return;
            }

            if pending_calls.is_empty() {
                break;
            }

            chat.messages.push(Message {
                reasoning_items: pending_reasoning_items,
                tool_calls: pending_calls.clone(),
                ..make_assistant_msg(
                    assistant_id,
                    model_name,
                    std::mem::take(&mut content),
                    std::mem::take(&mut thought),
                )
            });

            for call in pending_calls {
                let name = &call.function.name;
                let html = render_tool_activity(&call.function, None, true).into_string();
                if !send_ndjson(&tx, &json!({ "type": "tool_start", "html": html })).await {
                    return;
                }
                let output = if let Some(route) = mcp_routes.get(name) {
                    state
                        .mcp()
                        .execute_tool(&user.user_id, route, &call.function.arguments)
                        .await
                        .unwrap_or_else(|e| format!("tool error: {e}"))
                } else if let Some(&tool) = active_tools.iter().find(|tool| tool.name() == name) {
                    match tools::execute_builtin(
                        &state,
                        &effective,
                        (&user.user_id, &chat.id),
                        tool,
                        &call.function.arguments,
                        &tx,
                        req.browser_time_zone.as_ref(),
                    )
                    .await
                    {
                        Ok(outcome) => {
                            if outcome.shared_changed {
                                let _ =
                                    send_ndjson(&tx, &json!({ "type": "shared_updated" })).await;
                            }
                            outcome.output
                        }
                        Err(e) => format!("tool error: {e}"),
                    }
                } else {
                    format!("tool error: {name} is not enabled")
                };
                let _ = send_ndjson(
                    &tx,
                    &json!({ "type": "tool_end", "output": tool_preview(&output) }),
                )
                .await;

                chat.messages.push(Message {
                    id: new_id(),
                    role: Role::Tool,
                    content: output,
                    model_name: None,
                    reasoning: None,
                    reasoning_items: Vec::new(),
                    attachments: Vec::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call.id),
                    created_at: now_millis(),
                });
            }
            if !temporary {
                let _ = state.storage().save_chat(&user.user_id, &chat).await;
            }
            assistant_id = new_id();
            if !send_ndjson(
                &tx,
                &json!({ "type": "continue", "assistant_id": assistant_id }),
            )
            .await
            {
                return;
            }
        }

        let final_html = markdown::render(&content);
        chat.messages.push(make_assistant_msg(
            assistant_id.clone(),
            model_name,
            content.clone(),
            thought.clone(),
        ));
        chat.updated_at = now_millis();
        if !temporary {
            let _ = state.storage().save_chat(&user.user_id, &chat).await;
        }

        let _ = send_ndjson(
            &tx,
            &json!({
                "type": "done",
                "id": assistant_id,
                "html": final_html,
                "raw": content,
                "reasoning": thought,
            }),
        )
        .await;
    });

    let body_stream = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (b, rx)) });
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                header::HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            ),
        ],
        Body::from_stream(body_stream),
    )
        .into_response())
}

fn filter_allowed_mcp(
    cfg: &EffectiveConfig,
    requested: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, AppError> {
    if let Some(list) = &requested {
        for name in list {
            if !cfg.mcp.iter().any(|s| &s.name == name) {
                return Err(AppError::BadRequest(format!(
                    "MCP server '{name}' is not allowed"
                )));
            }
        }
    }
    Ok(requested)
}

async fn apply_stream_action(
    state: &AppState,
    user_id: &str,
    chat: &mut Chat,
    action: StreamAction,
) -> Result<Option<Value>, AppError> {
    match action {
        StreamAction::Send {
            content,
            attachments,
        } => {
            let trimmed = content.trim();
            let mut resolved_atts: Vec<Attachment> = Vec::with_capacity(attachments.len());
            for raw_name in &attachments {
                let att = files::resolve_shared_file_attachment(
                    state.storage(),
                    user_id,
                    &chat.id,
                    raw_name,
                )
                .await?;
                if !resolved_atts.iter().any(|a| a.name == att.name) {
                    resolved_atts.push(att);
                }
            }
            if trimmed.is_empty() && resolved_atts.is_empty() {
                return Err(AppError::BadRequest("message must not be empty".to_owned()));
            }
            if chat.messages.is_empty() && chat.title == "New chat" {
                let seed = if !trimmed.is_empty() {
                    trimmed
                } else {
                    resolved_atts[0].name.as_str()
                };
                chat.title = derive_title(seed);
            }
            let msg = Message {
                id: new_id(),
                role: Role::User,
                content: trimmed.to_owned(),
                model_name: None,
                reasoning: None,
                reasoning_items: Vec::new(),
                attachments: resolved_atts,
                tool_calls: Vec::new(),
                tool_call_id: None,
                created_at: now_millis(),
            };
            let ev = json!({
                "id": msg.id,
                "html": markdown::render(&msg.content),
                "raw": msg.content,
                "attachments": msg.attachments,
            });
            chat.messages.push(msg);
            Ok(Some(ev))
        }
        StreamAction::Edit {
            message_id,
            content,
        } => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Err(AppError::BadRequest("message must not be empty".to_owned()));
            }
            let idx = chat
                .messages
                .iter()
                .position(|m| m.id == message_id && m.role == Role::User)
                .ok_or_else(|| AppError::NotFound("user message not found".to_owned()))?;
            chat.messages[idx].content = trimmed.to_owned();
            chat.messages.truncate(idx + 1);
            if idx == 0 && chat.title == "New chat" {
                chat.title = derive_title(trimmed);
            }
            Ok(Some(json!({
                "id": chat.messages[idx].id,
                "html": markdown::render(trimmed),
                "raw": trimmed,
                "attachments": chat.messages[idx].attachments,
            })))
        }
        StreamAction::Retry { message_id } => {
            let idx = chat
                .messages
                .iter()
                .position(|m| m.id == message_id)
                .ok_or_else(|| AppError::NotFound("message not found".to_owned()))?;
            let keep = if chat.messages[idx].role == Role::User {
                idx + 1
            } else {
                idx
            };
            chat.messages.truncate(keep);
            if !chat.messages.iter().any(|m| m.role == Role::User) {
                return Err(AppError::BadRequest(
                    "nothing to retry without a user prompt".to_owned(),
                ));
            }
            Ok(None)
        }
    }
}

fn make_assistant_msg(id: String, model_name: &str, content: String, reasoning: String) -> Message {
    Message {
        id,
        role: Role::Assistant,
        content,
        model_name: Some(model_name.to_owned()),
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        reasoning_items: Vec::new(),
        attachments: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        created_at: now_millis(),
    }
}

async fn persist_assistant(
    state: &AppState,
    user_id: Option<&str>,
    chat: &mut Chat,
    id: &str,
    model_name: &str,
    content: &str,
    reasoning: &str,
) {
    let Some(user_id) = user_id else { return };
    if content.is_empty() && reasoning.is_empty() {
        return;
    }
    chat.messages.push(make_assistant_msg(
        id.to_owned(),
        model_name,
        content.to_owned(),
        reasoning.to_owned(),
    ));
    chat.updated_at = now_millis();
    let _ = state.storage().save_chat(user_id, chat).await;
}

async fn send_render(
    tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    content: &str,
    reasoning: &str,
) -> bool {
    send_ndjson(
        tx,
        &json!({
            "type": "render",
            "html": markdown::render(content),
            "reasoning": reasoning,
        }),
    )
    .await
}

async fn send_ndjson(tx: &mpsc::Sender<Result<Bytes, Infallible>>, val: &Value) -> bool {
    let mut vec = serde_json::to_vec(val).unwrap_or_default();
    vec.push(b'\n');
    tx.send(Ok(Bytes::from(vec))).await.is_ok()
}

fn derive_title(prompt: &str) -> String {
    prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(48).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "New chat".to_owned())
}
