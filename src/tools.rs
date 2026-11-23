use crate::{
    config::EffectiveConfig,
    domain::{BuiltinTool, ToolSpec},
    error::AppError,
    files,
    state::AppState,
    storage::new_id,
};
use axum::body::Bytes;
use chrono::{FixedOffset, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, convert::Infallible, time::Duration};
use tokio::sync::{Mutex, mpsc, oneshot};

struct PendingEntry {
    user_id: String,
    chat_id: String,
    tx: oneshot::Sender<String>,
}

#[derive(Default)]
pub struct PendingQuestions {
    entries: Mutex<HashMap<String, PendingEntry>>,
}

impl PendingQuestions {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(
        &self,
        user_id: &str,
        chat_id: &str,
    ) -> (String, oneshot::Receiver<String>) {
        let id = new_id();
        let (tx, rx) = oneshot::channel();
        self.entries.lock().await.insert(
            id.clone(),
            PendingEntry {
                user_id: user_id.to_owned(),
                chat_id: chat_id.to_owned(),
                tx,
            },
        );
        (id, rx)
    }

    pub async fn answer(
        &self,
        user_id: &str,
        chat_id: &str,
        question_id: &str,
        answer: String,
    ) -> Result<(), AppError> {
        let mut guard = self.entries.lock().await;
        let Some(existing) = guard.get(question_id) else {
            return Err(AppError::NotFound(
                "question expired or not found".to_owned(),
            ));
        };
        if existing.user_id != user_id || existing.chat_id != chat_id {
            return Err(AppError::NotFound("question not found".to_owned()));
        }
        if let Some(entry) = guard.remove(question_id) {
            let _ = entry.tx.send(answer);
        }
        Ok(())
    }

    pub async fn remove(&self, question_id: &str) {
        self.entries.lock().await.remove(question_id);
    }
}

pub struct BuiltinOutcome {
    pub output: String,
    pub shared_changed: bool,
}

pub fn builtin_tool_spec(tool: BuiltinTool) -> Option<ToolSpec> {
    let name = tool.name().to_owned();
    Some(match tool {
        BuiltinTool::CurrentTimestamp => ToolSpec {
            name,
            description: "Return the current UTC date, time, weekday, and Unix timestamp, plus browser-local time and time zone when available."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        BuiltinTool::Shared => ToolSpec {
            name,
            description: "Interact with the current chat's isolated shared folder (list, read, write, or delete files).".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "read", "write", "delete"],
                        "description": "Operation to perform on the current chat's shared folder."
                    },
                    "filename": {
                        "type": "string",
                        "description": "Target file name inside the shared folder (required for read, write, delete)."
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 text content to write when action is 'write'."
                    }
                },
                "required": ["action"]
            }),
        },
        BuiltinTool::AskUser => ToolSpec {
            name,
            description:
                "Ask the user a clarifying question interactively and wait for their response."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "Concise clarifying question for the user."
                    },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": 6,
                        "description": "Proposed answers for single or multiple choice. Omit for text."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["single", "multiple", "text"],
                        "description": "Use single to offer one selection (the default), multiple to offer one or more selections (returned as a JSON array), or text to offer no options. The user can always type a custom answer instead."
                    }
                },
                "required": ["question"]
            }),
        },
        BuiltinTool::Search => ToolSpec {
            name,
            description:
                "Search across your other conversations and extracted documents for matching text."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Case-insensitive search keywords."
                    }
                },
                "required": ["query"]
            }),
        },
        BuiltinTool::CurrentLocation => ToolSpec {
            name,
            description: "Request approximate browser coordinates. Use only when needed; the user must choose to share them.".to_owned(),
            parameters: json!({"type": "object", "properties": {}}),
        },
        BuiltinTool::ForkChat | BuiltinTool::SchedulePrompt => return None,
    })
}

#[derive(Deserialize)]
struct SharedArgs {
    action: String,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct AskUserArgs {
    question: String,
    mode: Option<AskUserMode>,
    #[serde(default)]
    options: Vec<String>,
}

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum AskUserMode {
    Single,
    Multiple,
    Text,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
}

#[derive(Deserialize)]
pub struct BrowserTimeZone {
    timezone: String,
    utc_offset_minutes: i16,
}

#[derive(Deserialize)]
struct BrowserLocation {
    latitude: f64,
    longitude: f64,
}

async fn ask_browser(
    state: &AppState,
    user_id: &str,
    chat_id: &str,
    mut event: Value,
    tx: &mpsc::Sender<Result<Bytes, Infallible>>,
) -> Result<Option<String>, AppError> {
    let (id, rx) = state.questions().register(user_id, chat_id).await;
    event["question_id"] = json!(id);
    let mut bytes = serde_json::to_vec(&event)?;
    bytes.push(b'\n');
    let answer = if tx.send(Ok(Bytes::from(bytes))).await.is_ok() {
        tokio::select! {
            answer = rx => answer.ok(),
            () = tx.closed() => None,
            () = tokio::time::sleep(Duration::from_secs(600)) => None,
        }
    } else {
        None
    };
    state.questions().remove(&id).await;
    Ok(answer)
}

pub async fn execute_builtin(
    state: &AppState,
    cfg: &EffectiveConfig,
    (user_id, chat_id): (&str, &str),
    tool: BuiltinTool,
    raw_args: &str,
    stream_tx: &mpsc::Sender<Result<Bytes, Infallible>>,
    browser_time_zone: Option<&BrowserTimeZone>,
) -> Result<BuiltinOutcome, AppError> {
    let text = |output: String| BuiltinOutcome {
        output,
        shared_changed: false,
    };
    match tool {
        BuiltinTool::CurrentTimestamp => {
            let now = Utc::now();
            let mut payload = json!({
                "utc": now.to_rfc3339_opts(SecondsFormat::Secs, true),
                "unix_seconds": now.timestamp(),
                "unix_millis": now.timestamp_millis(),
                "weekday": now.format("%A").to_string(),
            });
            if let Some(zone) = browser_time_zone.filter(|zone| {
                !zone.timezone.is_empty()
                    && zone.timezone.len() <= 64
                    && zone
                        .timezone
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/_+-".contains(&b))
                    && (-720..=840).contains(&zone.utc_offset_minutes)
            }) && let Some(offset) =
                FixedOffset::east_opt(i32::from(zone.utc_offset_minutes) * 60)
            {
                let local = now.with_timezone(&offset);
                payload["timezone"] = json!(zone.timezone);
                payload["utc_offset_minutes"] = json!(zone.utc_offset_minutes);
                payload["local"] = json!(local.to_rfc3339_opts(SecondsFormat::Secs, false));
                payload["local_weekday"] = json!(local.format("%A").to_string());
            }
            Ok(text(payload.to_string()))
        }
        BuiltinTool::Shared => execute_shared(state, cfg, user_id, chat_id, raw_args).await,
        BuiltinTool::AskUser => {
            let args: AskUserArgs = parse_json_args(raw_args)?;
            let question = args.question.trim();
            if question.is_empty() {
                return Err(AppError::BadRequest(
                    "question must not be empty".to_owned(),
                ));
            }
            let options: Vec<String> = args
                .options
                .into_iter()
                .map(|o| o.trim().to_owned())
                .filter(|o| !o.is_empty())
                .take(6)
                .collect();
            if matches!(args.mode, Some(AskUserMode::Single | AskUserMode::Multiple))
                && options.is_empty()
            {
                return Err(AppError::BadRequest(
                    "choice questions require at least one nonempty option".to_owned(),
                ));
            }

            let answer = ask_browser(
                state,
                user_id,
                chat_id,
                json!({
                    "type": "ask_user",
                    "question": question,
                    "options": options,
                    "mode": args.mode,
                }),
                stream_tx,
            )
            .await?;
            Ok(text(
                answer
                    .map(|ans| format!("User answered: {ans}"))
                    .unwrap_or_else(|| "No answer received.".to_owned()),
            ))
        }
        BuiltinTool::Search => {
            let args: SearchArgs = parse_json_args(raw_args)?;
            let hits = state
                .storage()
                .search_other_chats(user_id, chat_id, &args.query)
                .await?;
            Ok(BuiltinOutcome {
                output: serde_json::to_string(&hits)?,
                shared_changed: false,
            })
        }
        BuiltinTool::CurrentLocation => {
            let answer = ask_browser(
                state,
                user_id,
                chat_id,
                json!({"type": "current_location"}),
                stream_tx,
            )
            .await?;
            let Some(location) = answer
                .as_deref()
                .and_then(|raw| serde_json::from_str::<BrowserLocation>(raw).ok())
                .filter(|location| {
                    (-90.0..=90.0).contains(&location.latitude)
                        && (-180.0..=180.0).contains(&location.longitude)
                })
            else {
                return Ok(text("Browser location was not shared.".to_owned()));
            };
            Ok(text(
                json!({
                    "latitude": (location.latitude * 100.0).round() / 100.0,
                    "longitude": (location.longitude * 100.0).round() / 100.0,
                })
                .to_string(),
            ))
        }
        BuiltinTool::ForkChat | BuiltinTool::SchedulePrompt => {
            Err(AppError::NotFound("not a model tool".to_owned()))
        }
    }
}

async fn execute_shared(
    state: &AppState,
    cfg: &EffectiveConfig,
    user_id: &str,
    chat_id: &str,
    raw_args: &str,
) -> Result<BuiltinOutcome, AppError> {
    let args: SharedArgs = parse_json_args(raw_args)?;
    let storage = state.storage();

    match args.action.trim() {
        "list" => {
            let items = files::list_shared_files(storage, user_id, chat_id).await?;
            Ok(BuiltinOutcome {
                output: serde_json::to_string(&items)?,
                shared_changed: false,
            })
        }
        "read" => {
            let raw_name = args
                .filename
                .as_deref()
                .ok_or_else(|| AppError::BadRequest("filename is required for read".to_owned()))?;
            let safe_name = files::sanitize_filename(raw_name)?;
            let shared_path = storage.shared_dir(user_id, chat_id)?.join(&safe_name);
            let meta = tokio::fs::metadata(&shared_path)
                .await
                .map_err(|_| AppError::NotFound(format!("file '{safe_name}' not found")))?;

            let cache_path = storage
                .extracted_dir(user_id, chat_id)?
                .join(format!("{safe_name}.txt"));
            let extracted = match tokio::fs::read_to_string(&cache_path).await {
                Ok(t) => Some(t),
                Err(_) => files::extract_document_text(&shared_path).await,
            };

            let output = match extracted {
                Some(text) => {
                    let count = text.chars().count();
                    if count > 120_000 {
                        let head: String = text.chars().take(120_000).collect();
                        format!("{head}\n\n[Truncated at 120000 characters out of {count}]")
                    } else {
                        text
                    }
                }
                None => json!({
                    "name": safe_name,
                    "size": meta.len(),
                    "note": "binary or image file without extracted text"
                })
                .to_string(),
            };

            Ok(BuiltinOutcome {
                output,
                shared_changed: false,
            })
        }
        "write" => {
            let raw_name = args
                .filename
                .as_deref()
                .ok_or_else(|| AppError::BadRequest("filename is required for write".to_owned()))?;
            let safe_name = files::sanitize_filename(raw_name)?;
            let content = args.content.unwrap_or_default();
            if content.len() as u64 > cfg.max_file_size {
                return Err(AppError::PayloadTooLarge(format!(
                    "content exceeds max_file_size ({} bytes)",
                    cfg.max_file_size
                )));
            }

            let shared_dir = storage.shared_dir(user_id, chat_id)?;
            let extracted_dir = storage.extracted_dir(user_id, chat_id)?;
            tokio::fs::create_dir_all(&shared_dir).await?;
            tokio::fs::create_dir_all(&extracted_dir).await?;

            let tmp_file = shared_dir.join(format!(".write.{}.tmp", new_id()));
            tokio::fs::write(&tmp_file, content.as_bytes()).await?;
            let final_path = shared_dir.join(&safe_name);
            tokio::fs::rename(&tmp_file, &final_path).await?;

            let cache_path = extracted_dir.join(format!("{safe_name}.txt"));
            if files::detect_image_mime(&safe_name, None, content.as_bytes()).is_some() {
                let _ = tokio::fs::remove_file(&cache_path).await;
            } else {
                let tmp_txt = extracted_dir.join(format!(".ext.{}.tmp", new_id()));
                tokio::fs::write(&tmp_txt, content.as_bytes()).await?;
                tokio::fs::rename(&tmp_txt, &cache_path).await?;
            }

            Ok(BuiltinOutcome {
                output: json!({ "written": safe_name, "bytes": content.len() }).to_string(),
                shared_changed: true,
            })
        }
        "delete" => {
            let raw_name = args.filename.as_deref().ok_or_else(|| {
                AppError::BadRequest("filename is required for delete".to_owned())
            })?;
            let safe_name = files::sanitize_filename(raw_name)?;
            files::delete_shared_file(storage, user_id, chat_id, &safe_name).await?;
            Ok(BuiltinOutcome {
                output: json!({ "deleted": safe_name }).to_string(),
                shared_changed: true,
            })
        }
        other => Err(AppError::BadRequest(format!(
            "unsupported shared action '{other}'"
        ))),
    }
}

fn parse_json_args<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, AppError> {
    let trimmed = raw.trim();
    let effective = if trimmed.is_empty() { "{}" } else { trimmed };
    let val: Value = serde_json::from_str(effective)
        .map_err(|e| AppError::BadRequest(format!("invalid tool arguments: {e}")))?;
    serde_json::from_value(val)
        .map_err(|e| AppError::BadRequest(format!("invalid tool arguments: {e}")))
}
