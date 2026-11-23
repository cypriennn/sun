use crate::{
    domain::{BuiltinTool, Chat, Message, Role, UserContext},
    error::AppError,
    llm::{ChatCompletionRequest, LlmEvent, PreparedMessage},
    state::AppState,
    storage::{Storage, new_id, now_millis, validate_id},
};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};
use tokio::sync::{Mutex, Notify, OnceCell};

#[derive(Deserialize)]
pub struct ScheduleInput {
    pub prompt: String,
    pub run_at: DateTime<Utc>,
    pub every_minutes: Option<u32>,
}

impl ScheduleInput {
    pub fn validate(mut self) -> Result<Self, AppError> {
        self.prompt = self.prompt.trim().to_owned();
        if self.prompt.is_empty() || self.prompt.len() > 8192 {
            return Err(AppError::BadRequest(
                "scheduled prompt must contain 1–8192 bytes".to_owned(),
            ));
        }
        if self.run_at <= Utc::now() || self.every_minutes == Some(0) {
            return Err(AppError::BadRequest(
                "choose a future start time and an interval of at least one minute".to_owned(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    owner: String,
    pub prompt: String,
    pub next_run: Option<DateTime<Utc>>,
    pub every_minutes: Option<u32>,
    pub status: JobStatus,
    model: String,
    reasoning: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Started,
    Completed,
    Failed,
}

pub struct Scheduler {
    path: PathBuf,
    jobs: OnceCell<Mutex<Vec<Job>>>,
    wake: Notify,
}

impl Scheduler {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            path: data_dir.join("schedules.json"),
            jobs: OnceCell::new(),
            wake: Notify::new(),
        }
    }

    async fn jobs(&self) -> Result<tokio::sync::MutexGuard<'_, Vec<Job>>, AppError> {
        Ok(self
            .jobs
            .get_or_try_init(|| async {
                let jobs = Storage::read_json(&self.path).await?.unwrap_or_default();
                Ok::<_, AppError>(Mutex::new(jobs))
            })
            .await?
            .lock()
            .await)
    }

    async fn update<T>(
        &self,
        update: impl FnOnce(&mut Vec<Job>) -> Result<(T, bool), AppError>,
    ) -> Result<T, AppError> {
        let mut guard = self.jobs().await?;
        let mut jobs = guard.clone();
        let (result, changed) = update(&mut jobs)?;
        if changed {
            Storage::write_json_atomic(&self.path, &jobs).await?;
            *guard = jobs;
            self.wake.notify_one();
        }
        Ok(result)
    }

    pub async fn add(
        &self,
        owner: &str,
        model: &str,
        reasoning: Option<String>,
        input: ScheduleInput,
    ) -> Result<Job, AppError> {
        let input = input.validate()?;
        self.update(|jobs| {
            if jobs.iter().filter(|job| job.owner == owner).count() >= 32 {
                return Err(AppError::TooManyRequests(
                    "remove an existing schedule first (limit: 32)".to_owned(),
                ));
            }
            let job = Job {
                id: new_id(),
                owner: owner.to_owned(),
                prompt: input.prompt,
                next_run: Some(input.run_at),
                every_minutes: input.every_minutes,
                status: JobStatus::Pending,
                model: model.to_owned(),
                reasoning,
            };
            jobs.push(job.clone());
            Ok((job, true))
        })
        .await
    }

    pub async fn list(&self, owner: &str) -> Result<Vec<Job>, AppError> {
        Ok(self
            .jobs()
            .await?
            .iter()
            .filter(|job| job.owner == owner)
            .cloned()
            .collect())
    }

    pub async fn delete(&self, owner: &str, id: &str) -> Result<(), AppError> {
        validate_id(id)?;
        self.update(|jobs| {
            let at = jobs
                .iter()
                .position(|job| job.owner == owner && job.id == id)
                .ok_or_else(|| AppError::NotFound("schedule not found".to_owned()))?;
            jobs.remove(at);
            Ok(((), true))
        })
        .await
    }

    pub async fn run(state: AppState) {
        let scheduler = state.scheduler();
        loop {
            let now = Utc::now();
            let next = scheduler
                .jobs()
                .await
                .map(|jobs| jobs.iter().filter_map(|job| job.next_run).min());
            let wait = match next {
                Ok(Some(next)) if next <= now => {
                    if let Err(err) = scheduler.run_due(&state, now).await {
                        eprintln!("scheduler: {err}");
                    }
                    Duration::from_secs(10)
                }
                Ok(Some(next)) => (next - now)
                    .to_std()
                    .unwrap_or_default()
                    .min(Duration::from_secs(60)),
                Ok(None) => {
                    scheduler.wake.notified().await;
                    continue;
                }
                Err(err) => {
                    eprintln!("scheduler: {err}");
                    Duration::from_secs(60)
                }
            };
            tokio::select! {
                () = scheduler.wake.notified() => {},
                () = tokio::time::sleep(wait) => {},
            }
        }
    }

    async fn run_due(&self, state: &AppState, now: DateTime<Utc>) -> Result<(), AppError> {
        let due = self
            .jobs()
            .await?
            .iter()
            .filter(|job| job.next_run.is_some_and(|at| at <= now))
            .cloned()
            .collect::<Vec<_>>();
        for job in due {
            let cfg = state.config();
            let user = if cfg.auth.enabled {
                state.auth().active_user(&cfg.auth, &job.owner).await
            } else {
                (job.owner == "local").then(UserContext::local)
            };
            let Some(user) = user else { continue };
            let Ok(guard) = state.generation_guard(&job.owner).await else {
                continue;
            };
            let effective = state.effective_for(&user);
            if !effective
                .allowed_tools
                .contains(&BuiltinTool::SchedulePrompt)
            {
                continue;
            }
            let claimed = self
                .update(|jobs| {
                    let Some(entry) = jobs
                        .iter_mut()
                        .find(|entry| entry.id == job.id && entry.next_run == job.next_run)
                    else {
                        return Ok((false, false));
                    };
                    // Advance before calling the provider so a restart cannot repeat a paid run.
                    entry.next_run = next_run(entry.next_run, entry.every_minutes, Utc::now());
                    entry.status = JobStatus::Started;
                    Ok((true, true))
                })
                .await?;
            if !claimed {
                continue;
            }
            let state = state.clone();
            tokio::spawn(async move {
                let _guard = guard;
                let status = if run_prompt(&state, &user, &job).await.is_ok() {
                    JobStatus::Completed
                } else {
                    JobStatus::Failed
                };
                if let Err(err) = state
                    .scheduler()
                    .update(|jobs| {
                        let Some(entry) = jobs.iter_mut().find(|entry| entry.id == job.id) else {
                            return Ok(((), false));
                        };
                        entry.status = status;
                        Ok(((), true))
                    })
                    .await
                {
                    eprintln!("scheduler: {err}");
                }
            });
        }
        Ok(())
    }
}

fn next_run(
    previous: Option<DateTime<Utc>>,
    minutes: Option<u32>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let previous = previous?;
    let step = i64::from(minutes?.max(1)) * 60;
    let elapsed = (now - previous).num_seconds().max(0);
    previous.checked_add_signed(TimeDelta::try_seconds(
        (elapsed / step + 1).checked_mul(step)?,
    )?)
}

async fn run_prompt(state: &AppState, user: &UserContext, job: &Job) -> Result<(), AppError> {
    let cfg = state.config();
    let effective = state.effective_for(user);
    let prefs = state.storage().get_user_state(&user.user_id).await?;
    let (model, reasoning) = state.resolve_selection(
        &effective,
        Some(&job.model),
        Some(job.reasoning.as_deref().unwrap_or("")),
        &prefs,
        true,
    )?;
    let model =
        model.ok_or_else(|| AppError::BadRequest("scheduled model not available".to_owned()))?;
    let system = effective.resolve_system_prompt(&model.id, None);
    let mut rx = state
        .stream_completion(
            user,
            &effective,
            ChatCompletionRequest {
                base_url: &cfg.llm.base_url,
                api_key: state.secrets().llm_api_key.as_deref(),
                model: &model.id,
                reasoning: reasoning.as_deref(),
                system_prompt: system.as_deref(),
                messages: &[PreparedMessage {
                    role: Role::User,
                    text: job.prompt.clone(),
                    parts: Vec::new(),
                    reasoning_items: Vec::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                }],
                tools: &[],
                timeout: Some(Duration::from_secs(300)),
            },
        )
        .await?;
    let mut content = String::new();
    while let Some(event) = rx.recv().await {
        if let LlmEvent::Delta { content: delta, .. } = event? {
            content.push_str(&delta);
            if content.len() > 2 * 1024 * 1024 {
                return Err(AppError::Upstream(
                    "scheduled response too large".to_owned(),
                ));
            }
        }
    }
    if content.trim().is_empty() {
        return Err(AppError::Upstream(
            "scheduled response was empty".to_owned(),
        ));
    }
    let mut chat = Chat::new(model.id.clone(), reasoning);
    let title: String = job
        .prompt
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(64)
        .collect();
    chat.title = format!("Scheduled: {title}");
    chat.enabled_mcp = Some(Vec::new());
    chat.messages
        .push(Message::text(Role::User, job.prompt.clone()));
    chat.messages.push(Message {
        model_name: Some(model.display_name().to_owned()),
        ..Message::text(Role::Assistant, content)
    });
    chat.updated_at = now_millis();
    state.storage().save_chat(&user.user_id, &chat).await
}
