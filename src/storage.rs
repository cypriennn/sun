use crate::{
    domain::{BuiltinTool, Chat, ChatSummary, Role, UserState},
    error::AppError,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    cmp::Reverse,
    collections::{BTreeSet, HashMap},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, OwnedMutexGuard},
};

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct UserIndex {
    #[serde(default)]
    folders: BTreeSet<String>,
    #[serde(default)]
    chats: Vec<ChatSummary>,
}

#[derive(Debug)]
pub struct Storage {
    base_dir: PathBuf,
    indices: RwLock<HashMap<String, UserIndex>>,
    mutations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct TokenUsageRecord {
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    unreported: bool,
    #[serde(default)]
    pending: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SidebarData {
    pub folders: Vec<String>,
    pub chats: Vec<ChatSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchHit {
    pub chat_id: String,
    pub chat_title: String,
    pub source: String,
    pub snippet: String,
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub fn new_id() -> String {
    let ts = now_millis();
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut rnd = [0_u8; 5];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut rnd);
    }
    let mixed = u64::from_be_bytes([0, 0, 0, rnd[0], rnd[1], rnd[2], rnd[3], rnd[4]]) ^ seq;
    format!(
        "{:012x}{:010x}",
        ts & 0xffff_ffff_ffff,
        mixed & 0xff_ffff_ffff
    )
}

pub fn validate_id(id: &str) -> Result<(), AppError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(AppError::BadRequest("invalid identifier".to_owned()));
    }
    Ok(())
}

pub fn sanitize_folder(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 64 || trimmed.chars().any(char::is_control) {
        return Err(AppError::BadRequest("invalid folder name".to_owned()));
    }
    Ok(trimmed.to_owned())
}

impl Storage {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            indices: RwLock::new(HashMap::new()),
            mutations: Mutex::new(HashMap::new()),
        }
    }

    async fn mutation_guard(&self, user_id: &str) -> OwnedMutexGuard<()> {
        let lock = self
            .mutations
            .lock()
            .await
            .entry(user_id.to_owned())
            .or_default()
            .clone();
        lock.lock_owned().await
    }

    fn user_dir(&self, user_id: &str) -> Result<PathBuf, AppError> {
        validate_id(user_id)?;
        Ok(self.base_dir.join("users").join(user_id))
    }

    fn chat_dir(&self, user_id: &str, chat_id: &str) -> Result<PathBuf, AppError> {
        validate_id(chat_id)?;
        Ok(self.user_dir(user_id)?.join("chats").join(chat_id))
    }

    fn chat_path(&self, user_id: &str, chat_id: &str) -> Result<PathBuf, AppError> {
        Ok(self.chat_dir(user_id, chat_id)?.join("chat.json"))
    }

    pub fn shared_dir(&self, user_id: &str, chat_id: &str) -> Result<PathBuf, AppError> {
        Ok(self.chat_dir(user_id, chat_id)?.join("shared"))
    }

    pub fn extracted_dir(&self, user_id: &str, chat_id: &str) -> Result<PathBuf, AppError> {
        Ok(self.chat_dir(user_id, chat_id)?.join("extracted"))
    }

    pub(crate) async fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, AppError> {
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AppError::Internal(e.to_string())),
        }
    }

    pub(crate) async fn write_json_atomic<T: Serialize>(
        path: &Path,
        value: &T,
    ) -> Result<(), AppError> {
        if let Some(parent) = path.parent() {
            let mut builder = tokio::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700).create(parent).await?;
        }
        let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("data.json");
        let tmp = path.with_file_name(format!(".{name}.{seq}.tmp"));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(&tmp).await?;
        file.write_all(&serde_json::to_vec(value)?).await?;
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    async fn ensure_index(&self, user_id: &str) -> Result<UserIndex, AppError> {
        if let Some(existing) = self
            .indices
            .read()
            .map_err(|_| AppError::Internal("index lock poisoned".to_owned()))?
            .get(user_id)
            .cloned()
        {
            return Ok(existing);
        }

        let idx_path = self.user_dir(user_id)?.join("index.json");
        let loaded = match Self::read_json::<UserIndex>(&idx_path).await? {
            Some(idx) => idx,
            None => self.rebuild_index(user_id).await?,
        };

        self.indices
            .write()
            .map_err(|_| AppError::Internal("index lock poisoned".to_owned()))?
            .insert(user_id.to_owned(), loaded.clone());
        Ok(loaded)
    }

    async fn rebuild_index(&self, user_id: &str) -> Result<UserIndex, AppError> {
        let chats_dir = self.user_dir(user_id)?.join("chats");
        let mut index = UserIndex::default();
        let mut dir = match tokio::fs::read_dir(&chats_dir).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(index),
            Err(e) => return Err(AppError::Internal(e.to_string())),
        };
        while let Some(entry) = dir.next_entry().await? {
            if let Some(chat) = Self::read_json::<Chat>(&entry.path().join("chat.json")).await? {
                if let Some(folder) = &chat.folder {
                    index.folders.insert(folder.clone());
                }
                index.chats.push(ChatSummary {
                    id: chat.id,
                    title: chat.title,
                    folder: chat.folder,
                    updated_at: chat.updated_at,
                });
            }
        }
        index.chats.sort_by_key(|a| Reverse(a.updated_at));
        Ok(index)
    }

    async fn persist_index(&self, user_id: &str, index: UserIndex) -> Result<(), AppError> {
        let path = self.user_dir(user_id)?.join("index.json");
        Self::write_json_atomic(&path, &index).await?;
        self.indices
            .write()
            .map_err(|_| AppError::Internal("index lock poisoned".to_owned()))?
            .insert(user_id.to_owned(), index);
        Ok(())
    }

    pub async fn list_sidebar(&self, user_id: &str) -> Result<SidebarData, AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let idx = self.ensure_index(user_id).await?;
        Ok(SidebarData {
            folders: idx.folders.into_iter().collect(),
            chats: idx.chats,
        })
    }

    pub async fn get_user_state(&self, user_id: &str) -> Result<UserState, AppError> {
        let path = self.user_dir(user_id)?.join("state.json");
        Ok(Self::read_json(&path).await?.unwrap_or_default())
    }

    pub async fn check_token_limit(
        &self,
        user_id: &str,
        limit: Option<u64>,
    ) -> Result<(), AppError> {
        let Some(limit) = limit else {
            return Ok(());
        };
        let path = self.user_dir(user_id)?.join("usage.json");
        let usage = Self::read_json::<TokenUsageRecord>(&path)
            .await?
            .unwrap_or_default();
        if usage.unreported || usage.pending > 0 {
            return Err(AppError::TooManyRequests("token usage is unavailable for a previous request; quota enforcement requires provider usage reporting".to_owned()));
        }
        if usage.total_tokens >= limit {
            return Err(AppError::TooManyRequests(
                "token allowance exhausted".to_owned(),
            ));
        }
        Ok(())
    }

    pub async fn begin_token_usage(&self, user_id: &str) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let path = self.user_dir(user_id)?.join("usage.json");
        let mut record = Self::read_json::<TokenUsageRecord>(&path)
            .await?
            .unwrap_or_default();
        record.pending = record.pending.saturating_add(1);
        Self::write_json_atomic(&path, &record).await
    }

    pub async fn record_token_usage(
        &self,
        user_id: &str,
        delta: Option<u64>,
    ) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let path = self.user_dir(user_id)?.join("usage.json");
        let mut record = Self::read_json::<TokenUsageRecord>(&path)
            .await?
            .unwrap_or_default();
        record.total_tokens = record.total_tokens.saturating_add(delta.unwrap_or(0));
        record.unreported |= delta.is_none();
        record.pending = record.pending.saturating_sub(1);
        Self::write_json_atomic(&path, &record).await
    }

    pub async fn update_preferences(
        &self,
        user_id: &str,
        model: Option<String>,
        reasoning: Option<Option<String>>,
    ) -> Result<UserState, AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let mut state = self.get_user_state(user_id).await?;
        if let Some(m) = model {
            state.last_model = Some(m);
        }
        if let Some(r) = reasoning {
            state.last_reasoning = r;
        }
        let path = self.user_dir(user_id)?.join("state.json");
        Self::write_json_atomic(&path, &state).await?;
        Ok(state)
    }

    pub async fn update_mcp_state(
        &self,
        user_id: &str,
        server_name: &str,
        enabled: Option<bool>,
        auth_key: Option<Option<String>>,
    ) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let mut state = self.get_user_state(user_id).await?;
        let entry = state.mcp.entry(server_name.to_owned()).or_default();
        if let Some(en) = enabled {
            entry.enabled = Some(en);
        }
        if let Some(key) = auth_key {
            entry.auth_key = key;
        }
        let path = self.user_dir(user_id)?.join("state.json");
        Self::write_json_atomic(&path, &state).await
    }

    pub async fn update_tool_state(
        &self,
        user_id: &str,
        tool: BuiltinTool,
        enabled: bool,
    ) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let mut state = self.get_user_state(user_id).await?;
        state.tools.insert(tool.name().to_owned(), enabled);
        let path = self.user_dir(user_id)?.join("state.json");
        Self::write_json_atomic(&path, &state).await
    }

    pub async fn create_folder(&self, user_id: &str, name: &str) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let clean = sanitize_folder(name)?;
        let mut idx = self.ensure_index(user_id).await?;
        idx.folders.insert(clean);
        self.persist_index(user_id, idx).await
    }

    pub async fn rename_folder(
        &self,
        user_id: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let old_clean = sanitize_folder(old_name)?;
        let new_clean = sanitize_folder(new_name)?;
        let mut idx = self.ensure_index(user_id).await?;
        if !idx.folders.remove(&old_clean) {
            return Err(AppError::NotFound("folder not found".to_owned()));
        }
        idx.folders.insert(new_clean.clone());
        self.relocate_folder_chats(user_id, &mut idx, &old_clean, Some(new_clean))
            .await?;
        self.persist_index(user_id, idx).await
    }

    pub async fn delete_folder(&self, user_id: &str, name: &str) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        let clean = sanitize_folder(name)?;
        let mut idx = self.ensure_index(user_id).await?;
        idx.folders.remove(&clean);
        self.relocate_folder_chats(user_id, &mut idx, &clean, None)
            .await?;
        self.persist_index(user_id, idx).await
    }

    async fn relocate_folder_chats(
        &self,
        user_id: &str,
        idx: &mut UserIndex,
        from: &str,
        to: Option<String>,
    ) -> Result<(), AppError> {
        let mut affected = Vec::new();
        for summary in &mut idx.chats {
            if summary.folder.as_deref() == Some(from) {
                summary.folder.clone_from(&to);
                affected.push(summary.id.clone());
            }
        }
        for chat_id in affected {
            if let Ok(mut chat) = self.get_chat(user_id, &chat_id).await {
                chat.folder.clone_from(&to);
                Self::write_json_atomic(&self.chat_path(user_id, &chat_id)?, &chat).await?;
            }
        }
        Ok(())
    }

    pub async fn create_chat(
        &self,
        user_id: &str,
        model: String,
        reasoning: Option<String>,
        folder: Option<String>,
        enabled_mcp: Option<Vec<String>>,
    ) -> Result<Chat, AppError> {
        let folder = folder
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(sanitize_folder)
            .transpose()?;
        let chat = Chat {
            folder,
            enabled_mcp,
            ..Chat::new(model, reasoning)
        };
        self.save_chat(user_id, &chat).await?;
        Ok(chat)
    }

    pub async fn get_chat(&self, user_id: &str, chat_id: &str) -> Result<Chat, AppError> {
        Self::read_json(&self.chat_path(user_id, chat_id)?)
            .await?
            .ok_or_else(|| AppError::NotFound("chat not found".to_owned()))
    }

    pub async fn fork_chat(
        &self,
        user_id: &str,
        mut chat: Chat,
        through: &str,
    ) -> Result<ChatSummary, AppError> {
        let at = chat
            .messages
            .iter()
            .position(|msg| msg.id == through && matches!(msg.role, Role::User | Role::Assistant))
            .ok_or_else(|| AppError::NotFound("message not found".to_owned()))?;
        let tail = chat.messages[at + 1..]
            .iter()
            .take_while(|msg| msg.role == Role::Tool)
            .count();
        chat.messages.truncate(at + 1 + tail);
        let source = self.chat_dir(user_id, &chat.id)?;
        chat.id = new_id();
        chat.title = format!("Fork: {}", chat.title.chars().take(74).collect::<String>());
        chat.created_at = now_millis();
        chat.updated_at = chat.created_at;
        let dest = self.chat_dir(user_id, &chat.id)?;
        let result = async {
            for name in ["shared", "extracted"] {
                let mut entries = match tokio::fs::read_dir(source.join(name)).await {
                    Ok(entries) => entries,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err.into()),
                };
                let dir = dest.join(name);
                tokio::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&dir)
                    .await?;
                while let Some(entry) = entries.next_entry().await? {
                    if !entry.file_name().as_encoded_bytes().starts_with(b".")
                        && entry.file_type().await?.is_file()
                    {
                        tokio::fs::copy(entry.path(), dir.join(entry.file_name())).await?;
                    }
                }
            }
            self.save_chat(user_id, &chat).await
        }
        .await;
        if let Err(err) = result {
            let _ = tokio::fs::remove_dir_all(dest).await;
            return Err(err);
        }
        Ok(ChatSummary {
            id: chat.id,
            title: chat.title,
            folder: chat.folder,
            updated_at: chat.updated_at,
        })
    }

    pub async fn save_chat(&self, user_id: &str, chat: &Chat) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        Self::write_json_atomic(&self.chat_path(user_id, &chat.id)?, chat).await?;
        let mut idx = self.ensure_index(user_id).await?;
        if let Some(folder) = &chat.folder {
            idx.folders.insert(folder.clone());
        }
        let summary = ChatSummary {
            id: chat.id.clone(),
            title: chat.title.clone(),
            folder: chat.folder.clone(),
            updated_at: chat.updated_at,
        };
        if let Some(existing) = idx.chats.iter_mut().find(|c| c.id == chat.id) {
            *existing = summary;
        } else {
            idx.chats.push(summary);
        }
        idx.chats.sort_by_key(|a| Reverse(a.updated_at));
        self.persist_index(user_id, idx).await
    }

    pub async fn delete_chat(&self, user_id: &str, chat_id: &str) -> Result<(), AppError> {
        let _guard = self.mutation_guard(user_id).await;
        self.get_chat(user_id, chat_id).await?;
        match tokio::fs::remove_dir_all(self.chat_dir(user_id, chat_id)?).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AppError::Internal(e.to_string())),
        }
        let mut idx = self.ensure_index(user_id).await?;
        idx.chats.retain(|c| c.id != chat_id);
        self.persist_index(user_id, idx).await
    }

    pub async fn search_other_chats(
        &self,
        user_id: &str,
        current_chat_id: &str,
        query: &str,
    ) -> Result<Vec<SearchHit>, AppError> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let idx = self.ensure_index(user_id).await?;
        let mut hits = Vec::new();

        for summary in &idx.chats {
            if summary.id == current_chat_id {
                continue;
            }
            if let Ok(chat) = self.get_chat(user_id, &summary.id).await {
                for msg in &chat.messages {
                    if let Some(snippet) = match_snippet(&msg.content, &needle) {
                        hits.push(SearchHit {
                            chat_id: chat.id.clone(),
                            chat_title: chat.title.clone(),
                            source: format!("{:?}", msg.role).to_lowercase(),
                            snippet,
                        });
                        if hits.len() >= 10 {
                            return Ok(hits);
                        }
                    }
                }
            }
            if let Ok(ext_dir) = self.extracted_dir(user_id, &summary.id)
                && let Ok(mut rd) = tokio::fs::read_dir(ext_dir).await
            {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let fname = entry.file_name().to_string_lossy().into_owned();
                    if !fname.ends_with(".txt") || fname.starts_with('.') {
                        continue;
                    }
                    if let Ok(text) = tokio::fs::read_to_string(entry.path()).await
                        && let Some(snippet) = match_snippet(&text, &needle)
                    {
                        let orig = fname.strip_suffix(".txt").unwrap_or(&fname);
                        hits.push(SearchHit {
                            chat_id: summary.id.clone(),
                            chat_title: summary.title.clone(),
                            source: format!("file:{orig}"),
                            snippet,
                        });
                        if hits.len() >= 10 {
                            return Ok(hits);
                        }
                    }
                }
            }
        }
        Ok(hits)
    }
}

fn match_snippet(haystack: &str, needle_lower: &str) -> Option<String> {
    let lower = haystack.to_lowercase();
    let byte_pos = lower.find(needle_lower)?;
    let char_pos = lower[..byte_pos].chars().count();
    let start = char_pos.saturating_sub(60);
    let snippet: String = haystack.chars().skip(start).take(200).collect();
    Some(snippet.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn usage_and_preferences_survive_concurrent_changes_and_restarts() {
        let dir = PathBuf::from(format!("/tmp/opencode/sun-usage-{}", new_id()));
        let store = Arc::new(Storage::new(dir.clone()));
        let mut tasks = Vec::new();
        for i in 0..10 {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                store.begin_token_usage("alice").await.expect("begin");
                store
                    .record_token_usage("alice", Some(10))
                    .await
                    .expect("usage");
                store
                    .update_mcp_state("alice", &format!("mcp-{i}"), Some(true), None)
                    .await
                    .expect("preferences");
            }));
        }
        for task in tasks {
            task.await.expect("task");
        }
        let restarted = Storage::new(dir.clone());
        assert!(
            restarted
                .check_token_limit("alice", Some(100))
                .await
                .is_err()
        );
        assert!(
            restarted
                .check_token_limit("alice", Some(101))
                .await
                .is_ok()
        );
        assert!(restarted.check_token_limit("bob", Some(1)).await.is_ok());
        assert_eq!(
            restarted
                .get_user_state("alice")
                .await
                .expect("state")
                .mcp
                .len(),
            10
        );
        restarted.begin_token_usage("bob").await.expect("pending");
        assert!(restarted.check_token_limit("bob", Some(1)).await.is_err());
        restarted
            .record_token_usage("bob", None)
            .await
            .expect("unreported");
        restarted
            .record_token_usage("bob", Some(1))
            .await
            .expect("known usage");
        assert!(
            restarted
                .check_token_limit("bob", Some(1_000))
                .await
                .is_err()
        );
        let state: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(dir.join("users/alice/state.json"))
                .await
                .expect("read"),
        )
        .expect("json");
        assert!(state.get("total_tokens").is_none());
        tokio::fs::remove_dir_all(dir).await.expect("cleanup");
    }

    #[tokio::test]
    async fn isolated_per_user_and_per_chat_storage() {
        let tmp = PathBuf::from(format!("/tmp/opencode/sun-test-{}", new_id()));
        let store = Storage::new(tmp.clone());

        let chat_a = store
            .create_chat(
                "alice",
                "gpt-4.1-mini".to_owned(),
                None,
                Some("ops".to_owned()),
                None,
            )
            .await
            .expect("create alice chat");
        let _chat_b = store
            .create_chat("bob", "gpt-4.1-mini".to_owned(), None, None, None)
            .await
            .expect("create bob chat");

        assert!(store.get_chat("bob", &chat_a.id).await.is_err());

        let alice_sidebar = store.list_sidebar("alice").await.expect("alice sidebar");
        assert_eq!(alice_sidebar.folders, vec!["ops".to_owned()]);
        assert_eq!(alice_sidebar.chats.len(), 1);

        store
            .delete_chat("alice", &chat_a.id)
            .await
            .expect("delete");
        assert!(store.get_chat("alice", &chat_a.id).await.is_err());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
