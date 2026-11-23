use crate::{
    config::{Config, EffectiveConfig, ModelConfig},
    domain::{
        Attachment, AttachmentKind, BuiltinTool, Chat, ChatSummary, Message, Role, SharedFile,
        ToolFunctionCall,
    },
    markdown,
    schedule::{Job, JobStatus},
    storage::SidebarData,
    web::assets::brand_icon_url,
};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use std::borrow::Cow;

pub struct McpViewItem {
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub has_key: bool,
}

pub fn render_schedules(jobs: &[Job]) -> Markup {
    html! {
        @if jobs.is_empty() {
            p class="runtime-hint" { "No scheduled prompts." }
        }
        @for job in jobs {
            details {
                summary {
                    (match job.status {
                        JobStatus::Pending => "Pending",
                        JobStatus::Started => "Started (not retried if interrupted)",
                        JobStatus::Completed => "Completed · see chats",
                        JobStatus::Failed => "Failed",
                    })
                    @if let Some(minutes) = job.every_minutes { " · every " (minutes) " min" }
                    @else { " · once" }
                    @if let Some(next) = job.next_run {
                        " · next " time datetime=(next.to_rfc3339()) { (next.to_rfc3339()) }
                    }
                }
                pre class="tool-detail" { (&job.prompt) }
                button type="button" data-action="delete-schedule" data-id=(job.id) { "Remove schedule" }
            }
        }
    }
}

pub struct PageView<'a> {
    pub config: &'a Config,
    pub effective: &'a EffectiveConfig,
    pub sidebar: &'a SidebarData,
    pub chat: Option<&'a Chat>,
    pub shared_files: &'a [SharedFile],
    pub builtin_tools: &'a [(BuiltinTool, bool)],
    pub mcp_views: &'a [McpViewItem],
    pub models: &'a [ModelConfig],
    pub active_model: Option<&'a str>,
    pub active_reasoning: Option<&'a str>,
}

pub fn render_page(view: &PageView<'_>) -> Markup {
    let branding = &view.config.branding;
    let accent = branding.safe_accent();
    let background = branding.safe_background();
    let panel = branding.safe_panel();
    let brand_name = &branding.name;
    let brand_icon = brand_icon_url(branding);
    let page_title = match view.chat {
        Some(c) => format!("{} · {brand_name}", c.title),
        None => brand_name.clone(),
    };
    let chat_id = view.chat.map(|c| c.id.as_str()).unwrap_or("");
    let chat_title = view.chat.map(|c| c.title.as_str()).unwrap_or("New chat");
    let can_fork = view
        .effective
        .allowed_tools
        .contains(&BuiltinTool::ForkChat);
    html! {
        (DOCTYPE)
        html lang="en" style=(format!("--accent: {accent}; --bg: {background}; --panel: {panel};")) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
                meta name="theme-color" content=(background);
                meta name="mobile-web-app-capable" content="yes";
                meta name="apple-mobile-web-app-capable" content="yes";
                meta name="apple-mobile-web-app-status-bar-style" content="black-translucent";
                meta name="apple-mobile-web-app-title" content=(brand_name);
                title { (page_title) }
                link rel="icon" href=(brand_icon);
                link rel="apple-touch-icon" href=(brand_icon);
                link rel="manifest" href="/manifest.webmanifest";
                link rel="stylesheet" href="/style.css";
            }
            body {
                div id="backdrop" class="backdrop" {}
                div id="app" class="app" data-chat-id=(chat_id) data-can-fork=(can_fork) {
                    aside id="sidebar" class="sidebar" aria-label="Chats and folders" tabindex="-1" {
                        a href="/" class="brand" {
                            img class="brand-logo" src=(brand_icon) alt="";
                            span { (brand_name) }
                        }
                        div class="side-actions" {
                            a href="/" class="btn-primary" { "+ New chat" }
                            button type="button" id="new-folder-btn" title="New folder" { "+ Folder" }
                        }
                        form id="folder-form" class="folder-form" hidden {
                            input type="text" id="folder-name-input" placeholder="Folder name" maxlength="64";
                            button type="submit" { "Add" }
                        }
                        nav class="nav-list" {
                            @for folder in &view.sidebar.folders {
                                (render_folder(folder, &view.sidebar.chats, chat_id))
                            }
                            div id="root-chats" {
                                @for item in view.sidebar.chats.iter().filter(|c| c.folder.is_none()) {
                                    (render_chat_item(item, chat_id))
                                }
                            }
                        }
                    }

                    main id="main-pane" class="main" {
                        header class="topbar" {
                            button type="button" id="menu-btn" class="drawer-btn" aria-label="Chats and folders" aria-controls="sidebar" aria-expanded="false" { "☰" }
                            input
                                type="text"
                                id="chat-title"
                                class="chat-title-input"
                                value=(chat_title)
                                aria-label="Chat title";
                            label id="temporary-label" class="tool-label" hidden[view.chat.is_some()] title=(format!("Not saved in {brand_name}. Still sent to the model provider.")) {
                                input type="checkbox" id="temporary-chat" autocomplete="off";
                                "Temporary"
                            }
                            button type="button" id="runtime-btn" class="drawer-btn" aria-controls="runtime-panel" aria-expanded="false" { "Runtime" }
                        }

                        section id="messages" class="messages" {
                            @if let Some(chat) = view.chat {
                                @if chat.messages.is_empty() {
                                    div id="empty-state" class="empty-state" { (brand_name) }
                                } @else {
                                    @for (i, msg) in chat.messages.iter().enumerate() {
                                        @if is_visible_message(msg) {
                                            (render_message(&chat.id, msg, &chat.messages[i + 1..], can_fork))
                                        }
                                    }
                                }
                            } @else {
                                div id="empty-state" class="empty-state" { (brand_name) }
                            }
                        }

                        div id="error-banner" class="error-banner" role="alert" hidden {}

                        div class="composer-wrap" {
                            div id="attachment-slot" class="att-slot" hidden {}
                            form id="composer" class="composer" {
                                input type="file" id="file-input" multiple hidden;
                                button type="button" id="attach-btn" class="attach-btn" title="Attach files" { "+" }
                                textarea
                                    id="prompt-input"
                                    rows="1"
                                    autocomplete="off"
                                    placeholder="Send a message..."
                                    autofocus
                                {}
                                button type="submit" id="send-btn" class="send-btn" { "Send" }
                                button type="button" id="stop-btn" class="stop-btn" hidden { "Stop" }
                            }
                        }
                    }
                    (render_runtime(view, chat_id))
                }
                script src="/app.js" defer {}
            }
        }
    }
}

fn render_runtime(view: &PageView<'_>, chat_id: &str) -> Markup {
    let current_folder = view.chat.and_then(|c| c.folder.as_deref()).unwrap_or("");
    let chat_sys_prompt = view
        .chat
        .and_then(|c| c.system_prompt.as_deref())
        .unwrap_or("");
    let active_mcp_count = view.mcp_views.iter().filter(|m| m.enabled).count();

    html! {
        aside id="runtime-panel" class="runtime-panel" aria-labelledby="runtime-heading" tabindex="-1" {
            header class="runtime-header" {
                h2 id="runtime-heading" { "Runtime" }
                button type="button" id="runtime-close" class="drawer-btn" aria-label="Close runtime panel" { "×" }
            }
            div id="runtime-error" class="error-banner" role="alert" hidden {}

            @if !view.models.is_empty() {
                div class="runtime-group" {
                    label for="model-select" { "Model" }
                    select id="model-select" {
                        @for model in view.models {
                            @let r_tags = serde_json::to_string(model.effective_reasoning_tags(&view.effective.reasoning_tags)).unwrap_or_else(|_| "[]".to_owned());
                            @let badges = serde_json::to_string(&model.tags.badge_list()).unwrap_or_else(|_| "[]".to_owned());
                            option
                                value=(model.id)
                                selected[view.active_model == Some(model.id.as_str())]
                                data-reasoning=(r_tags)
                                data-default-reasoning=(model.default_reasoning.as_deref().unwrap_or(""))
                                data-tags=(badges)
                            { (model.display_name()) }
                        }
                    }
                    span id="model-tags" class="model-tags" {}
                }
                div id="reasoning-group" class="runtime-group" hidden {
                    label for="reasoning-select" { "Reasoning" }
                    select id="reasoning-select" data-initial=(view.active_reasoning.unwrap_or("")) {}
                }
            }
            @if !view.sidebar.folders.is_empty() {
                div class="runtime-group" {
                    label for="chat-folder" { "Folder" }
                    select id="chat-folder" {
                        option value="" selected[current_folder.is_empty()] { "No folder" }
                        @for folder in &view.sidebar.folders {
                            option value=(folder) selected[current_folder == folder] { (folder) }
                        }
                    }
                }
            }
            details class="runtime-section" open[!chat_sys_prompt.is_empty()] {
                summary { "System prompt" }
                div class="runtime-group" {
                    textarea
                        id="system-prompt"
                        autocomplete="off"
                        aria-label="Custom system prompt"
                        placeholder="Optional custom system prompt for this chat..."
                    { (chat_sys_prompt) }
                    button type="button" id="save-sys" hidden[view.chat.is_none()] { "Save" }
                }
            }
            details id="shared-bar" class="runtime-section" open[!view.shared_files.is_empty()] {
                summary { "Shared files (" span id="shared-count" { (view.shared_files.len()) } ")" }
                div class="runtime-group" {
                    div id="shared-list" class="shared-list" {
                        @for sf in view.shared_files {
                            (render_shared_file(chat_id, sf))
                        }
                    }
                    input type="file" id="shared-file-input" multiple hidden;
                    button type="button" id="shared-upload-btn" { "+ Upload to shared" }
                }
            }
            details class="runtime-section" open {
                summary { "Built-in tools" }
                div class="runtime-group" {
                    @for &(tool, enabled) in view.builtin_tools {
                        @if let Some(label) = tool.label() {
                            label class="tool-label" {
                                input type="checkbox" class="tool-toggle" data-tool=(tool.name()) checked[enabled];
                                (label)
                            }
                        }
                    }
                    button type="button" id="compact-btn" disabled[view.chat.is_none()] { "Compact memory" }
                }
            }
            @let can_schedule = view.effective.allowed_tools.contains(&BuiltinTool::SchedulePrompt);
            details id="schedules" class="runtime-section" {
                summary { "Automations" }
                @if can_schedule {
                    p class="runtime-hint" { "Runs without tools. Repeats use a fixed interval; missed runs are combined. Results appear as new chats." @if view.config.auth.enabled { " Requires an active sign-in." } }
                    form id="schedule-form" class="runtime-group" {
                        textarea name="prompt" placeholder="Prompt to run" aria-label="Scheduled prompt" required maxlength="8192" {}
                        label for="schedule-at" { "First run (local time)" }
                        input id="schedule-at" name="run_at" type="datetime-local" required;
                        label for="schedule-interval" { "Repeat every (minutes, blank for once)" }
                        input id="schedule-interval" name="every_minutes" type="number" min="1" max="4294967295";
                        button type="submit" { "Schedule" }
                    }
                }
                div id="schedule-list" class="runtime-group" {}
            }
            @if !view.mcp_views.is_empty() {
                details class="runtime-section" {
                    summary { "MCP (" span id="mcp-active-count" { (active_mcp_count) } "/" (view.mcp_views.len()) ")" }
                    div class="runtime-group" {
                        @for m in view.mcp_views {
                            div class="mcp-row" {
                                label class="tool-label" {
                                    input type="checkbox" class="mcp-toggle" data-mcp=(m.name) checked[m.enabled];
                                    (&m.name)
                                }
                                span class="tag mcp-url" title=(m.url) { (short_host(&m.url)) }
                                div class="mcp-key-controls" {
                                    input
                                        type="password"
                                        class="mcp-key"
                                        data-mcp=(m.name)
                                        aria-label=(format!("Bearer auth key for {}", m.name))
                                        placeholder=(if m.has_key { "Key set (type to update)" } else { "Bearer auth key" })
                                        autocomplete="off";
                                    button type="button" class="mcp-save-btn" data-action="save-mcp-key" { "Save" }
                                    button
                                        type="button"
                                        class="icon-btn danger mcp-clear-key"
                                        data-action="clear-mcp-key"
                                        data-mcp=(m.name)
                                        hidden[!m.has_key]
                                        aria-label="Clear auth key"
                                    { "×" }
                                }
                            }
                        }
                    }
                }
            }
            @if !view.builtin_tools.is_empty() || !view.mcp_views.is_empty() {
                p class="runtime-hint" { "Tool choices are saved for all your chats." }
            }
            @if view.config.auth.enabled {
                form class="runtime-actions" method="post" action="/auth/logout" {
                    button type="submit" { "Sign out" }
                }
            }
        }
    }
}

fn is_visible_message(msg: &Message) -> bool {
    match msg.role {
        Role::User => true,
        Role::Assistant => {
            !msg.content.is_empty() || msg.reasoning.is_some() || !msg.tool_calls.is_empty()
        }
        Role::System | Role::Tool => false,
    }
}

fn short_host(url: &str) -> &str {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

fn render_shared_file(chat_id: &str, sf: &SharedFile) -> Markup {
    let href = file_href(chat_id, &sf.name);
    let size_prefix = match sf.kind {
        AttachmentKind::Image => "vision · ",
        AttachmentKind::Document => "",
        AttachmentKind::File => "file · ",
    };
    html! {
        div class="shared-item" data-name=(sf.name) {
            a class="shared-link" href=(href) download=(sf.name) { (&sf.name) }
            span class="tag" { (size_prefix) (format_bytes(sf.size)) }
            button type="button" class="icon-btn" data-action="attach-shared" data-name=(sf.name) title="Attach to message" { "+" }
            button type="button" class="icon-btn danger" data-action="delete-shared" data-name=(sf.name) title="Delete file" { "×" }
        }
    }
}

fn render_folder(folder: &str, chats: &[ChatSummary], active_chat_id: &str) -> Markup {
    html! {
        details class="folder" open {
            summary {
                span class="folder-name" { "📁 " (folder) }
                span class="folder-btns" {
                    button type="button" class="icon-btn" data-action="new-in-folder" data-folder=(folder) title="New chat in folder" { "+" }
                    button type="button" class="icon-btn" data-action="rename-folder" data-folder=(folder) title="Rename folder" { "✎" }
                    button type="button" class="icon-btn danger" data-action="delete-folder" data-folder=(folder) title="Delete folder" { "×" }
                }
            }
            div class="folder-chats" {
                @for item in chats.iter().filter(|c| c.folder.as_deref() == Some(folder)) {
                    (render_chat_item(item, active_chat_id))
                }
            }
        }
    }
}

fn render_chat_item(item: &ChatSummary, active_chat_id: &str) -> Markup {
    let is_active = item.id == active_chat_id;
    html! {
        div class=(if is_active { "chat-item active" } else { "chat-item" }) data-chat-id=(item.id) {
            a class="chat-link" href=(format!("/c/{}", item.id)) { (item.title) }
            span class="chat-btns" {
                button
                    type="button"
                    class="icon-btn danger"
                    data-action="delete-chat"
                    data-id=(item.id)
                    title="Delete chat"
                { "×" }
            }
        }
    }
}

fn render_message(chat_id: &str, msg: &Message, following: &[Message], can_fork: bool) -> Markup {
    let is_user = msg.role == Role::User;
    let role_class = if is_user {
        "msg msg-user"
    } else {
        "msg msg-assistant"
    };
    let role_label = if is_user { "You" } else { "Assistant" };
    let rendered_html = markdown::render(&msg.content);
    let reasoning = msg.reasoning.as_deref().unwrap_or("");

    html! {
        article class=(role_class) data-msg-id=(msg.id) {
            div class="msg-role" {
                (role_label)
                @if let Some(name) = &msg.model_name {
                    span class="msg-model" { " (" (name) ")" }
                }
            }
            div class="msg-card" {
                @if !is_user {
                    div class="msg-activity" {
                        @if !reasoning.is_empty() {
                            details class="activity reasoning" {
                                summary { "Reasoning" }
                                div class="activity-text" { (reasoning) }
                            }
                        }
                        @for call in &msg.tool_calls {
                            @let output = following.iter().take_while(|m| m.role == Role::Tool)
                                .find(|m| m.tool_call_id.as_deref() == Some(call.id.as_str()))
                                .map(|m| m.content.as_str());
                            (render_tool_activity(&call.function, output, false))
                        }
                    }
                }
                @if !msg.attachments.is_empty() {
                    div class="msg-atts" {
                        @for att in &msg.attachments {
                            (render_attachment(chat_id, att))
                        }
                    }
                }
                div class="msg-body" { (PreEscaped(rendered_html)) }
                textarea class="msg-raw" hidden { (&msg.content) }
            }
            @if msg.tool_calls.is_empty() {
                div class="msg-actions" {
                    button type="button" data-action="copy-msg" { "Copy" }
                    @if is_user {
                        button type="button" data-action="edit-msg" { "Edit" }
                    }
                    button type="button" data-action="retry-msg" { "Retry" }
                    @if can_fork {
                        button type="button" data-action="fork-chat" { "Fork here" }
                    }
                }
            }
        }
    }
}

pub(super) fn render_tool_activity(
    call: &ToolFunctionCall,
    output: Option<&str>,
    running: bool,
) -> Markup {
    let result = output.unwrap_or(if running {
        "Waiting for result…"
    } else {
        "No result recorded."
    });
    html! {
        details class="activity tool-activity" data-tool-name=(call.name) {
            summary { @if running { "Calling " (call.name) "…" } @else { "Tool: " (call.name) } }
            div { "Arguments" }
            pre class="activity-text" { (tool_preview(&call.arguments)) }
            div { "Result" }
            pre class="activity-text tool-output" { (tool_preview(result)) }
        }
    }
}

pub(super) fn tool_preview(text: &str) -> Cow<'_, str> {
    const LIMIT: usize = 4096;
    if text.len() <= LIMIT {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!(
            "{}\n… (truncated)",
            &text[..text.floor_char_boundary(LIMIT)]
        ))
    }
}

fn render_attachment(chat_id: &str, att: &Attachment) -> Markup {
    let href = file_href(chat_id, &att.name);
    if att.kind == AttachmentKind::Image {
        html! {
            a class="att-img-link" href=(href) target="_blank" rel="noopener" {
                img class="att-img" src=(href) alt=(att.name);
            }
        }
    } else {
        html! {
            a class="att-chip" href=(href) download=(att.name) { (&att.name) }
        }
    }
}

fn file_href(chat_id: &str, name: &str) -> String {
    format!("/api/chats/{chat_id}/files/{}", url_encode_segment(name))
}

fn url_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn format_bytes(size: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if size >= MIB {
        format!("{:.1} MB", size as f64 / MIB as f64)
    } else if size >= KIB {
        format!("{:.1} KB", size as f64 / KIB as f64)
    } else {
        format!("{size} B")
    }
}
