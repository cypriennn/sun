use crate::{
    auth::AuthManager,
    config::{Config, EffectiveConfig, ModelConfig, Secrets},
    domain::{UserContext, UserState},
    error::AppError,
    llm::{ChatCompletionRequest, LlmClient, LlmEvent},
    mcp::McpHub,
    schedule::Scheduler,
    storage::Storage,
    tools::PendingQuestions,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc};

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    config_path: PathBuf,
    config: RwLock<Arc<Config>>,
    secrets: Secrets,
    pub storage: Storage,
    pub llm: LlmClient,
    pub mcp: McpHub,
    pub questions: PendingQuestions,
    scheduler: Scheduler,
    auth: AuthManager,
    generations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    discovered_models: RwLock<Vec<String>>,
}

impl AppState {
    pub fn new(config_path: PathBuf, config: Config, secrets: Secrets) -> Result<Self, AppError> {
        let storage = Storage::new(config.server.data_dir.clone());
        let scheduler = Scheduler::new(&config.server.data_dir);
        let llm = LlmClient::new()?;
        let inner = AppStateInner {
            config_path,
            config: RwLock::new(Arc::new(config)),
            secrets,
            storage,
            llm,
            mcp: McpHub::new(),
            questions: PendingQuestions::new(),
            scheduler,
            auth: AuthManager::new()?,
            generations: Mutex::new(HashMap::new()),
            discovered_models: RwLock::new(Vec::new()),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn config(&self) -> Arc<Config> {
        self.inner
            .config
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|e| Arc::clone(&e.into_inner()))
    }

    pub fn secrets(&self) -> &Secrets {
        &self.inner.secrets
    }

    pub fn storage(&self) -> &Storage {
        &self.inner.storage
    }

    pub fn llm(&self) -> &LlmClient {
        &self.inner.llm
    }

    pub fn mcp(&self) -> &McpHub {
        &self.inner.mcp
    }

    pub fn questions(&self) -> &PendingQuestions {
        &self.inner.questions
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.inner.scheduler
    }

    pub fn auth(&self) -> &AuthManager {
        &self.inner.auth
    }

    pub async fn generation_guard(&self, user_id: &str) -> Result<OwnedMutexGuard<()>, AppError> {
        let lock = self
            .inner
            .generations
            .lock()
            .await
            .entry(user_id.to_owned())
            .or_default()
            .clone();
        lock.try_lock_owned().map_err(|_| {
            AppError::TooManyRequests("a response is already running for this user".to_owned())
        })
    }

    pub fn effective_for(&self, user: &UserContext) -> EffectiveConfig {
        let cfg = self.config();
        let discovered = self
            .inner
            .discovered_models
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        cfg.effective_for(user, &discovered)
    }

    pub async fn stream_completion(
        &self,
        user: &UserContext,
        cfg: &EffectiveConfig,
        request: ChatCompletionRequest<'_>,
    ) -> Result<mpsc::Receiver<Result<LlmEvent, AppError>>, AppError> {
        let limit = cfg.max_tokens_limit();
        self.storage()
            .check_token_limit(&user.user_id, limit)
            .await?;
        let mut upstream = self.llm().stream_chat(request).await?;
        self.storage().begin_token_usage(&user.user_id).await?;
        let (tx, rx) = mpsc::channel(64);
        let state = self.clone();
        let uid = user.user_id.clone();
        tokio::spawn(async move {
            let mut usage = None;
            while let Some(event) = upstream.recv().await {
                match event {
                    Ok(LlmEvent::Usage(tokens)) => usage = Some(tokens),
                    other => {
                        let _ = tx.send(other).await;
                    }
                }
            }
            if let Err(err) = state.storage().record_token_usage(&uid, usage).await {
                let _ = tx.send(Err(err)).await;
            } else if limit.is_some() && usage.is_none() {
                let _ = tx.send(Err(AppError::Upstream("provider did not report token usage; further requests are blocked by the quota".to_owned()))).await;
            }
        });
        Ok(rx)
    }

    pub fn resolve_selection(
        &self,
        cfg: &EffectiveConfig,
        requested_model: Option<&str>,
        requested_reasoning: Option<&str>,
        user_state: &UserState,
        strict: bool,
    ) -> Result<(Option<ModelConfig>, Option<String>), AppError> {
        let models = &cfg.models;
        if models.is_empty() {
            if strict && requested_model.is_some() {
                return Err(AppError::BadRequest(
                    "no models configured in config.toml".to_owned(),
                ));
            }
            return Ok((None, None));
        }

        let chosen = if let Some(req_id) = requested_model.filter(|s| !s.is_empty()) {
            match models.iter().find(|m| m.id == req_id) {
                Some(m) => m.clone(),
                None if strict => {
                    return Err(AppError::BadRequest(format!(
                        "model '{req_id}' is not allowed by config.toml"
                    )));
                }
                None => self.default_model(cfg, models, user_state),
            }
        } else {
            self.default_model(cfg, models, user_state)
        };

        let allowed_tags = chosen.effective_reasoning_tags(&cfg.reasoning_tags);
        let reasoning = if allowed_tags.is_empty() {
            None
        } else if let Some(req_r) = requested_reasoning {
            if req_r.is_empty() {
                None
            } else if allowed_tags.iter().any(|t| t == req_r) {
                Some(req_r.to_owned())
            } else if strict {
                return Err(AppError::BadRequest(format!(
                    "reasoning tag '{req_r}' is not allowed for model '{}'",
                    chosen.id
                )));
            } else {
                chosen
                    .default_reasoning
                    .clone()
                    .filter(|d| allowed_tags.contains(d))
            }
        } else {
            user_state
                .last_reasoning
                .as_ref()
                .filter(|r| allowed_tags.contains(r))
                .cloned()
                .or_else(|| {
                    chosen
                        .default_reasoning
                        .clone()
                        .filter(|d| allowed_tags.contains(d))
                })
        };

        Ok((Some(chosen), reasoning))
    }

    fn default_model(
        &self,
        cfg: &EffectiveConfig,
        models: &[ModelConfig],
        user_state: &UserState,
    ) -> ModelConfig {
        user_state
            .last_model
            .as_deref()
            .and_then(|id| models.iter().find(|m| m.id == id))
            .or_else(|| {
                cfg.default_model
                    .as_deref()
                    .and_then(|id| models.iter().find(|m| m.id == id))
            })
            .or_else(|| models.first())
            .cloned()
            .expect("non-empty models")
    }

    pub async fn refresh_discovered_models(&self) {
        let cfg = self.config();
        if !cfg.any_auto_discover() {
            if let Ok(mut guard) = self.inner.discovered_models.write() {
                guard.clear();
            }
            return;
        }
        if let Ok(ids) = self
            .inner
            .llm
            .discover_models(&cfg.llm.base_url, self.inner.secrets.llm_api_key.as_deref())
            .await
            && let Ok(mut guard) = self.inner.discovered_models.write()
        {
            *guard = ids;
        }
    }

    pub fn spawn_config_watcher(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut last_sig = file_signature(&state.inner.config_path).await;
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                let sig = file_signature(&state.inner.config_path).await;
                if sig.is_none() || sig == last_sig {
                    continue;
                }
                last_sig = sig;
                match Config::load(&state.inner.config_path).await {
                    Ok(new_cfg) => {
                        let prev = state.config();
                        let need_discover = new_cfg.any_auto_discover()
                            && (!prev.any_auto_discover()
                                || prev.llm.base_url != new_cfg.llm.base_url);
                        let clear_discover = !new_cfg.any_auto_discover();
                        if new_cfg.auth != prev.auth {
                            state.inner.auth.invalidate().await;
                        }
                        state
                            .inner
                            .mcp
                            .prune_stale_urls(&new_cfg.all_configured_mcp())
                            .await;
                        if let Ok(mut guard) = state.inner.config.write() {
                            *guard = Arc::new(new_cfg);
                        }
                        if need_discover {
                            state.refresh_discovered_models().await;
                        } else if clear_discover
                            && let Ok(mut guard) = state.inner.discovered_models.write()
                        {
                            guard.clear();
                        }
                    }
                    Err(err) => {
                        eprintln!("config reload skipped: {err}");
                    }
                }
            }
        });
    }
}

async fn file_signature(path: &PathBuf) -> Option<(Option<SystemTime>, u64)> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    Some((meta.modified().ok(), meta.len()))
}
