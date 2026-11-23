use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct UserContext {
    pub user_id: String,
    pub groups: Vec<String>,
}

impl UserContext {
    pub fn local() -> Self {
        Self {
            user_id: "local".to_owned(),
            groups: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserMcpState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_key: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, UserMcpState>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinTool {
    CurrentTimestamp,
    Shared,
    AskUser,
    Search,
    CurrentLocation,
    ForkChat,
    SchedulePrompt,
}

impl BuiltinTool {
    pub const ALL: [Self; 7] = [
        Self::CurrentTimestamp,
        Self::Shared,
        Self::AskUser,
        Self::Search,
        Self::CurrentLocation,
        Self::ForkChat,
        Self::SchedulePrompt,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::CurrentTimestamp => "current_timestamp",
            Self::Shared => "shared",
            Self::AskUser => "ask_user",
            Self::Search => "search",
            Self::CurrentLocation => "current_location",
            Self::ForkChat => "fork_chat",
            Self::SchedulePrompt => "schedule_prompt",
        }
    }

    pub fn label(self) -> Option<&'static str> {
        Some(match self {
            Self::CurrentTimestamp => "Current date, time, and time zone",
            Self::Shared => "Access shared files",
            Self::AskUser => "Ask follow-up questions",
            Self::Search => "Search other chats",
            Self::CurrentLocation => "Approximate browser location",
            Self::ForkChat | Self::SchedulePrompt => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Document,
    File,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub kind: AttachmentKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedFile {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub kind: AttachmentKind,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunctionCall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_items: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub created_at: u64,
}

impl Message {
    pub fn text(role: Role, content: String) -> Self {
        Self {
            id: crate::storage::new_id(),
            role,
            content,
            model_name: None,
            reasoning: None,
            reasoning_items: Vec::new(),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            created_at: crate::storage::now_millis(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_mcp: Option<Vec<String>>,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<Message>,
}

impl Chat {
    pub fn new(model: String, reasoning: Option<String>) -> Self {
        let now = crate::storage::now_millis();
        Self {
            id: crate::storage::new_id(),
            title: "New chat".to_owned(),
            folder: None,
            system_prompt: None,
            model,
            reasoning,
            enabled_mcp: None,
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub updated_at: u64,
}
