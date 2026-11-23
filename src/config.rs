use crate::{
    domain::{BuiltinTool, UserContext, UserState},
    error::AppError,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Secrets {
    pub llm_api_key: Option<String>,
    pub oidc_client_secret: Option<String>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mask = |v: &Option<String>| v.as_ref().map(|_| "[REDACTED]");
        f.debug_struct("Secrets")
            .field("llm_api_key", &mask(&self.llm_api_key))
            .field("oidc_client_secret", &mask(&self.oidc_client_secret))
            .finish()
    }
}

impl Secrets {
    pub fn from_env() -> Self {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        Self {
            llm_api_key: get("SUN_LLM_API_KEY"),
            oidc_client_secret: get("SUN_OIDC_CLIENT_SECRET"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub branding: BrandingConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub files: FilesConfig,
    #[serde(default)]
    pub prompts: PromptsConfig,
    pub llm: LlmConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub allowed_tools: Option<Vec<BuiltinTool>>,
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,
    #[serde(default)]
    pub groups: BTreeMap<String, GroupConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            data_dir: default_data_dir(),
        }
    }
}

const DEFAULT_ACCENT_COLOR: &str = "#4f8cff";
const DEFAULT_BACKGROUND_COLOR: &str = "#0d0f12";
const DEFAULT_PANEL_COLOR: &str = "#13161b";

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrandingConfig {
    pub name: String,
    pub logo: Option<String>,
    pub accent_color: String,
    pub background_color: String,
    pub panel_color: String,
}

impl Default for BrandingConfig {
    fn default() -> Self {
        Self {
            name: "sun".to_owned(),
            logo: None,
            accent_color: DEFAULT_ACCENT_COLOR.to_owned(),
            background_color: DEFAULT_BACKGROUND_COLOR.to_owned(),
            panel_color: DEFAULT_PANEL_COLOR.to_owned(),
        }
    }
}

impl BrandingConfig {
    pub fn safe_accent(&self) -> &str {
        safe_color(&self.accent_color, DEFAULT_ACCENT_COLOR)
    }

    pub fn safe_background(&self) -> &str {
        safe_color(&self.background_color, DEFAULT_BACKGROUND_COLOR)
    }

    pub fn safe_panel(&self) -> &str {
        safe_color(&self.panel_color, DEFAULT_PANEL_COLOR)
    }
}

fn safe_color<'a>(color: &'a str, fallback: &'static str) -> &'a str {
    let color = color.trim();
    let valid = color
        .strip_prefix('#')
        .is_some_and(|h| matches!(h.len(), 3 | 6) && h.bytes().all(|b| b.is_ascii_hexdigit()));
    if valid { color } else { fallback }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub issuer_url: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub redirect_url: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
    #[serde(default = "default_session_ttl")]
    pub session_ttl_seconds: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer_url: None,
            client_id: None,
            redirect_url: None,
            scopes: default_scopes(),
            groups_claim: default_groups_claim(),
            session_ttl_seconds: default_session_ttl(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesConfig {
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            max_file_size: default_max_file_size(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptsConfig {
    #[serde(default)]
    pub developer_preprompt: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    pub base_url: String,
    #[serde(default)]
    pub auto_discover_models: bool,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub reasoning_tags: Vec<String>,
    #[serde(default)]
    pub max_m_tokens: Option<f64>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub reasoning_tags: Vec<String>,
    #[serde(default)]
    pub default_reasoning: Option<String>,
    #[serde(default)]
    pub tags: ModelTags,
}

impl ModelConfig {
    pub fn display_name(&self) -> &str {
        clean_opt(self.name.as_deref()).unwrap_or(&self.id)
    }

    pub fn effective_reasoning_tags<'a>(&'a self, fallback: &'a [String]) -> &'a [String] {
        if self.reasoning_tags.is_empty() {
            fallback
        } else {
            &self.reasoning_tags
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTags {
    #[serde(default)]
    pub smartness: Option<String>,
    #[serde(default)]
    pub pace: Option<String>,
    #[serde(default)]
    pub price: Option<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl ModelTags {
    pub fn badge_list(&self) -> Vec<(&str, &str)> {
        let mut out = Vec::new();
        for (k, v) in [
            ("smartness", self.smartness.as_deref()),
            ("pace", self.pace.as_deref()),
            ("price", self.price.as_deref()),
        ] {
            if let Some(val) = clean_opt(v) {
                out.push((k, val));
            }
        }
        for (k, v) in &self.extra {
            if let Some(val) = clean_opt(Some(v)) {
                out.push((k.as_str(), val));
            }
        }
        out
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub current_timestamp: bool,
    #[serde(default = "default_true")]
    pub shared: bool,
    #[serde(default = "default_true")]
    pub ask_user: bool,
    #[serde(default)]
    pub search: bool,
    #[serde(default)]
    pub current_location: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            current_timestamp: true,
            shared: true,
            ask_user: true,
            search: false,
            current_location: false,
        }
    }
}

impl ToolsConfig {
    fn enabled_by_default(&self, tool: BuiltinTool) -> bool {
        match tool {
            BuiltinTool::CurrentTimestamp => self.current_timestamp,
            BuiltinTool::Shared => self.shared,
            BuiltinTool::AskUser => self.ask_user,
            BuiltinTool::Search => self.search,
            BuiltinTool::CurrentLocation => self.current_location,
            BuiltinTool::ForkChat | BuiltinTool::SchedulePrompt => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub enabled_by_default: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupToolsConfig {
    #[serde(default)]
    pub current_timestamp: Option<bool>,
    #[serde(default)]
    pub shared: Option<bool>,
    #[serde(default)]
    pub ask_user: Option<bool>,
    #[serde(default)]
    pub search: Option<bool>,
    pub current_location: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupConfig {
    #[serde(default)]
    pub max_file_size: Option<u64>,
    #[serde(default)]
    pub max_m_tokens: Option<f64>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub auto_discover_models: Option<bool>,
    #[serde(default)]
    pub reasoning_tags: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    #[serde(default)]
    pub models: Option<Vec<ModelConfig>>,
    #[serde(default)]
    pub allowed_mcp: Option<Vec<String>>,
    #[serde(default)]
    pub mcp: Option<Vec<McpServerConfig>>,
    #[serde(default)]
    pub prompts: PromptsConfig,
    #[serde(default)]
    pub tools: GroupToolsConfig,
    #[serde(default)]
    pub allowed_tools: Option<Vec<BuiltinTool>>,
}

#[derive(Clone, Debug)]
pub struct EffectiveConfig {
    pub max_file_size: u64,
    pub max_m_tokens: Option<f64>,
    pub default_model: Option<String>,
    pub reasoning_tags: Vec<String>,
    pub models: Vec<ModelConfig>,
    pub mcp: Vec<McpServerConfig>,
    pub prompts: PromptsConfig,
    pub tools: ToolsConfig,
    pub allowed_tools: Vec<BuiltinTool>,
}

impl EffectiveConfig {
    pub fn builtin_tools<'a>(
        &'a self,
        user_state: &'a UserState,
    ) -> impl Iterator<Item = (BuiltinTool, bool)> + 'a {
        self.allowed_tools
            .iter()
            .copied()
            .filter(|tool| tool.label().is_some())
            .map(|tool| {
                let enabled = user_state
                    .tools
                    .get(tool.name())
                    .copied()
                    .unwrap_or_else(|| self.tools.enabled_by_default(tool));
                (tool, enabled)
            })
    }

    pub fn resolve_enabled_mcp(
        &self,
        user_state: &UserState,
        chat_selection: Option<&[String]>,
    ) -> Vec<McpServerConfig> {
        self.mcp
            .iter()
            .filter(|s| {
                user_state
                    .mcp
                    .get(&s.name)
                    .and_then(|u| u.enabled)
                    .or_else(|| chat_selection.map(|list| list.contains(&s.name)))
                    .unwrap_or(s.enabled_by_default)
            })
            .cloned()
            .collect()
    }

    pub fn resolve_system_prompt(
        &self,
        model_id: &str,
        chat_prompt: Option<&str>,
    ) -> Option<String> {
        let model_sys = self
            .models
            .iter()
            .find(|m| m.id == model_id)
            .and_then(|m| clean_opt(m.system_prompt.as_deref()));
        let base_sys = model_sys.or_else(|| clean_opt(self.prompts.system_prompt.as_deref()));
        let sections: Vec<&str> = [
            clean_opt(self.prompts.developer_preprompt.as_deref()),
            base_sys,
            clean_opt(chat_prompt),
        ]
        .into_iter()
        .flatten()
        .collect();

        (!sections.is_empty()).then(|| sections.join("\n\n"))
    }

    pub fn max_tokens_limit(&self) -> Option<u64> {
        self.max_m_tokens
            .filter(|&m| m > 0.0 && m.is_finite())
            .map(|m| (m * 1_000_000.0) as u64)
    }
}

fn default_bind() -> String {
    "0.0.0.0:3030".to_owned()
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}
fn default_max_file_size() -> u64 {
    26_214_400
}
fn default_scopes() -> Vec<String> {
    vec![
        "openid".to_owned(),
        "profile".to_owned(),
        "email".to_owned(),
    ]
}
fn default_groups_claim() -> String {
    "groups".to_owned()
}
fn default_session_ttl() -> u64 {
    28_800
}
fn default_true() -> bool {
    true
}

fn clean_opt(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|v| !v.is_empty())
}

fn is_http_url(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("http://") || u.starts_with("https://")
}

fn has_forbidden_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    ["secret", "api_key", "apikey", "password", "token", "bearer"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn validate_models(models: &[ModelConfig]) -> Result<(), AppError> {
    for model in models {
        if model.id.trim().is_empty() {
            return Err(AppError::BadRequest(
                "model id must not be empty".to_owned(),
            ));
        }
        if model.tags.extra.keys().any(|k| has_forbidden_key(k)) {
            return Err(AppError::BadRequest(
                "secrets are forbidden in config.toml".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_mcp_servers(servers: &[McpServerConfig]) -> Result<(), AppError> {
    let mut seen = BTreeSet::new();
    for server in servers {
        let name = server.name.trim();
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(AppError::BadRequest(
                "mcp server name must be 1..=32 ASCII alphanumeric, '-' or '_' characters"
                    .to_owned(),
            ));
        }
        if !seen.insert(name) {
            return Err(AppError::BadRequest(format!(
                "duplicate mcp server name '{name}'"
            )));
        }
        if !is_http_url(&server.url) {
            return Err(AppError::BadRequest(format!(
                "mcp server '{}' must use http:// or https:// streamable transport",
                server.name
            )));
        }
    }
    Ok(())
}

impl Config {
    pub fn parse_toml(raw: &str) -> Result<Self, AppError> {
        let cfg: Self = toml::from_str(raw)
            .map_err(|e| AppError::BadRequest(format!("invalid config: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load_sync(path: &Path) -> Result<Self, AppError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| AppError::Internal(format!("failed reading {}: {e}", path.display())))?;
        Self::parse_toml(&raw)
    }

    pub async fn load(path: &Path) -> Result<Self, AppError> {
        let raw = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| AppError::Internal(format!("failed reading {}: {e}", path.display())))?;
        Self::parse_toml(&raw)
    }

    fn validate(&self) -> Result<(), AppError> {
        if !is_http_url(&self.llm.base_url) {
            return Err(AppError::BadRequest(
                "llm.base_url must start with http:// or https://".to_owned(),
            ));
        }
        if self.files.max_file_size == 0 {
            return Err(AppError::BadRequest(
                "files.max_file_size must be greater than zero".to_owned(),
            ));
        }
        if let Some(m) = self.llm.max_m_tokens
            && (!m.is_finite() || m < 0.0)
        {
            return Err(AppError::BadRequest(
                "llm.max_m_tokens must be a non-negative finite number".to_owned(),
            ));
        }
        if self.auth.enabled
            && (!self.auth.issuer_url.as_deref().is_some_and(is_http_url)
                || clean_opt(self.auth.client_id.as_deref()).is_none()
                || !self.auth.redirect_url.as_deref().is_some_and(is_http_url)
                || self.auth.session_ttl_seconds == 0)
        {
            return Err(AppError::BadRequest(
                "auth requires valid issuer_url, client_id, redirect_url, and session_ttl_seconds when enabled"
                    .to_owned(),
            ));
        }
        if self.auth.enabled {
            let issuer =
                openidconnect::url::Url::parse(self.auth.issuer_url.as_deref().unwrap_or_default())
                    .map_err(|_| AppError::BadRequest("invalid auth.issuer_url".to_owned()))?;
            let redirect = openidconnect::url::Url::parse(
                self.auth.redirect_url.as_deref().unwrap_or_default(),
            )
            .map_err(|_| AppError::BadRequest("invalid auth.redirect_url".to_owned()))?;
            if !crate::auth::secure_url(&issuer)
                || !crate::auth::secure_url(&redirect)
                || issuer.query().is_some()
                || redirect.query().is_some()
                || redirect.path() != "/auth/callback"
                || self.auth.groups_claim.trim().is_empty()
            {
                return Err(AppError::BadRequest("OIDC URLs require HTTPS (or loopback HTTP), and redirect_url must end in /auth/callback without a query".to_owned()));
            }
        }
        validate_models(&self.llm.models)?;
        validate_mcp_servers(&self.mcp)?;

        for (gname, g) in &self.groups {
            if gname.trim().is_empty() {
                return Err(AppError::BadRequest(
                    "group name must not be empty".to_owned(),
                ));
            }
            if g.max_file_size == Some(0) {
                return Err(AppError::BadRequest(format!(
                    "group '{gname}' max_file_size must be greater than zero"
                )));
            }
            if let Some(m) = g.max_m_tokens
                && (!m.is_finite() || m < 0.0)
            {
                return Err(AppError::BadRequest(format!(
                    "group '{gname}' max_m_tokens must be non-negative"
                )));
            }
            if let Some(models) = &g.models {
                validate_models(models)?;
            }
            if let Some(mcp) = &g.mcp {
                validate_mcp_servers(mcp)?;
            }
        }
        Ok(())
    }

    pub fn all_configured_mcp(&self) -> Vec<McpServerConfig> {
        let mut all = self.mcp.clone();
        for g in self.groups.values() {
            if let Some(gmcp) = &g.mcp {
                for s in gmcp {
                    if !all
                        .iter()
                        .any(|existing| existing.name == s.name && existing.url == s.url)
                    {
                        all.push(s.clone());
                    }
                }
            }
        }
        all
    }

    pub fn any_auto_discover(&self) -> bool {
        self.llm.auto_discover_models
            || (self.auth.enabled
                && self
                    .groups
                    .values()
                    .any(|g| g.auto_discover_models == Some(true)))
    }

    pub fn effective_for(&self, user: &UserContext, discovered: &[String]) -> EffectiveConfig {
        let mut max_file_size = self.files.max_file_size;
        let mut max_m_tokens = self.llm.max_m_tokens;
        let mut default_model = self.llm.default_model.clone();
        let mut auto_discover = self.llm.auto_discover_models;
        let mut reasoning_tags = self.llm.reasoning_tags.clone();
        let mut base_models = self.llm.models.clone();
        let mut allowed_models: Option<Vec<String>> = None;
        let mut mcp = self.mcp.clone();
        let mut allowed_mcp: Option<Vec<String>> = None;
        let mut prompts = self.prompts.clone();
        let mut tools = self.tools.clone();
        let mut allowed_tools = self.allowed_tools.as_deref();

        if self.auth.enabled {
            for (group_name, g) in &self.groups {
                if !user.groups.contains(group_name) {
                    continue;
                }
                if let Some(fs) = g.max_file_size {
                    max_file_size = fs;
                }
                if let Some(mt) = g.max_m_tokens {
                    max_m_tokens = (mt > 0.0).then_some(mt);
                }
                if let Some(dm) = &g.default_model {
                    default_model = Some(dm.clone());
                }
                if let Some(ad) = g.auto_discover_models {
                    auto_discover = ad;
                }
                if let Some(rt) = &g.reasoning_tags {
                    reasoning_tags.clone_from(rt);
                }
                if let Some(gm) = &g.models {
                    base_models.clone_from(gm);
                }
                if let Some(am) = &g.allowed_models {
                    allowed_models = Some(am.clone());
                }
                if let Some(gmcp) = &g.mcp {
                    mcp.clone_from(gmcp);
                }
                if let Some(amcp) = &g.allowed_mcp {
                    allowed_mcp = Some(amcp.clone());
                }
                if let Some(dp) = &g.prompts.developer_preprompt {
                    prompts.developer_preprompt = Some(dp.clone());
                }
                if let Some(sp) = &g.prompts.system_prompt {
                    prompts.system_prompt = Some(sp.clone());
                }
                if let Some(v) = g.tools.current_timestamp {
                    tools.current_timestamp = v;
                }
                if let Some(v) = g.tools.shared {
                    tools.shared = v;
                }
                if let Some(v) = g.tools.ask_user {
                    tools.ask_user = v;
                }
                if let Some(v) = g.tools.search {
                    tools.search = v;
                }
                if let Some(v) = g.tools.current_location {
                    tools.current_location = v;
                }
                if let Some(allow) = &g.allowed_tools {
                    allowed_tools = Some(allow);
                }
            }
        }

        if auto_discover {
            for id in discovered {
                if !base_models.iter().any(|m| m.id == *id) {
                    base_models.push(ModelConfig {
                        id: id.clone(),
                        name: None,
                        system_prompt: None,
                        reasoning_tags: reasoning_tags.clone(),
                        default_reasoning: None,
                        tags: ModelTags::default(),
                    });
                }
            }
        }

        if let Some(allow) = allowed_models {
            base_models.retain(|m| allow.iter().any(|id| id == &m.id));
        }

        if let Some(allow) = allowed_mcp {
            mcp.retain(|s| allow.iter().any(|name| name == &s.name));
        }

        EffectiveConfig {
            max_file_size,
            max_m_tokens,
            default_model,
            reasoning_tags,
            models: base_models,
            mcp,
            prompts,
            tools,
            allowed_tools: BuiltinTool::ALL
                .into_iter()
                .filter(|tool| allowed_tools.is_none_or(|allow| allow.contains(tool)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branding_colors_default_and_reject_invalid_hex() {
        let branding: BrandingConfig = toml::from_str("").expect("default branding");
        assert_eq!(branding.safe_accent(), "#4f8cff");
        assert_eq!(branding.safe_background(), "#0d0f12");
        assert_eq!(branding.safe_panel(), "#13161b");

        let branding: BrandingConfig = toml::from_str(
            "accent_color = '#red'\nbackground_color = ' #ABC '\npanel_color = '#123; color: red'",
        )
        .expect("custom branding");
        assert_eq!(branding.safe_accent(), "#4f8cff");
        assert_eq!(branding.safe_background(), "#ABC");
        assert_eq!(branding.safe_panel(), "#13161b");
    }

    #[test]
    fn group_precedence_is_deterministic_and_disabled_oidc_ignores_groups() {
        let mut cfg = Config::parse_toml(
            r#"
[llm]
base_url = "https://llm.example/v1"
[[llm.models]]
id = "global-model"
[groups.alpha]
max_file_size = 64
allowed_mcp = []
[groups.zeta]
max_file_size = 128
[groups.zeta.tools]
search = true
"#,
        )
        .expect("config");
        let user = UserContext {
            user_id: "alice".to_owned(),
            groups: vec!["zeta".to_owned(), "alpha".to_owned()],
        };
        assert_eq!(
            cfg.effective_for(&user, &[]).max_file_size,
            default_max_file_size()
        );
        cfg.auth.enabled = true;
        let first = cfg.effective_for(&user, &[]);
        let other = UserContext {
            groups: vec!["alpha".to_owned(), "zeta".to_owned()],
            ..user
        };
        assert_eq!(first.max_file_size, 128);
        assert_eq!(
            first.max_file_size,
            cfg.effective_for(&other, &[]).max_file_size
        );
        assert!(first.tools.search);
    }

    #[test]
    fn rejects_secrets_and_non_http_mcp_in_config_toml() {
        for raw in [
            "[llm]\nbase_url = \"https://api.openai.com/v1\"\napi_key = \"sk-secret\"\n",
            "[auth]\nenabled = true\nclient_secret = \"sec\"\n[llm]\nbase_url = \"https://api.openai.com/v1\"\n",
            "[llm]\nbase_url = \"https://api.openai.com/v1\"\n[[mcp]]\nname = \"ops\"\nurl = \"https://mcp.example.com\"\ntoken = \"sec\"\n",
            "[llm]\nbase_url = \"https://api.openai.com/v1\"\n[[mcp]]\nname = \"ops\"\nurl = \"stdio:///usr/bin/mcp\"\n",
            "[llm]\nbase_url = \"https://api.openai.com/v1\"\n[[mcp]]\nname = \"ops\"\ncommand = \"npx\"\nurl = \"https://mcp.example.com\"\n",
            "[llm]\nbase_url = \"https://api.openai.com/v1\"\n[[mcp]]\nname = \"ops\"\ntransport = \"sse\"\nurl = \"https://mcp.example.com\"\n",
        ] {
            assert!(Config::parse_toml(raw).is_err());
        }
    }

    #[test]
    fn model_system_prompt_and_oidc_group_overrides_global() {
        let raw = r#"
[auth]
enabled = true
issuer_url = "https://idp.example.com"
client_id = "sun"
redirect_url = "https://sun.example.com/auth/callback"

[prompts]
developer_preprompt = "DEV_RULE"
system_prompt = "GLOBAL_PROMPT"

[llm]
base_url = "https://api.openai.com/v1"
max_m_tokens = 2.5

[[llm.models]]
id = "m1"

[[llm.models]]
id = "m2"
system_prompt = "MODEL_PROMPT"

[[mcp]]
name = "github"
url = "https://mcp.example.com/gh"

[[mcp]]
name = "grafana"
url = "https://mcp.example.com/gf"

[groups.sre]
max_m_tokens = 50.0
allowed_models = ["m2"]
allowed_mcp = ["grafana"]

[groups.sre.tools]
search = true
"#;
        let cfg = Config::parse_toml(raw).expect("valid config");
        let regular_user = UserContext {
            user_id: "u_1".to_owned(),
            groups: vec!["viewer".to_owned()],
        };
        let eff_reg = cfg.effective_for(&regular_user, &[]);
        assert_eq!(eff_reg.max_tokens_limit(), Some(2_500_000));
        assert_eq!(eff_reg.models.len(), 2);
        assert_eq!(eff_reg.mcp.len(), 2);
        assert!(!eff_reg.tools.search);
        assert_eq!(
            eff_reg.resolve_system_prompt("m1", None).as_deref(),
            Some("DEV_RULE\n\nGLOBAL_PROMPT")
        );

        let sre_user = UserContext {
            user_id: "u_2".to_owned(),
            groups: vec!["sre".to_owned()],
        };
        let eff_sre = cfg.effective_for(&sre_user, &[]);
        assert_eq!(eff_sre.max_tokens_limit(), Some(50_000_000));
        assert_eq!(eff_sre.models.len(), 1);
        assert_eq!(eff_sre.models[0].id, "m2");
        assert_eq!(eff_sre.mcp.len(), 1);
        assert_eq!(eff_sre.mcp[0].name, "grafana");
        assert!(eff_sre.tools.search);
        assert_eq!(
            eff_sre
                .resolve_system_prompt("m2", Some("CHAT_PROMPT"))
                .as_deref(),
            Some("DEV_RULE\n\nMODEL_PROMPT\n\nCHAT_PROMPT")
        );
    }
}
