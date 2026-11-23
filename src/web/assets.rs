use crate::{config::BrandingConfig, markdown::push_escaped, state::AppState};
use axum::{
    extract::State,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

const ICON_PATH: &str = "/icon.svg";

fn configured_logo(branding: &BrandingConfig) -> Option<&str> {
    branding
        .logo
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn is_image_url(logo: &str) -> bool {
    logo.starts_with("http://") || logo.starts_with("https://") || logo.starts_with('/')
}

pub fn brand_icon_url(branding: &BrandingConfig) -> &str {
    configured_logo(branding)
        .filter(|logo| is_image_url(logo))
        .unwrap_or(ICON_PATH)
}

fn generated_icon_svg(branding: &BrandingConfig) -> String {
    let panel = branding.safe_panel();
    let mark = match configured_logo(branding).filter(|logo| !is_image_url(logo)) {
        Some(logo) => {
            let size = (64 / logo.chars().count()).clamp(1, 42);
            let mut text = String::new();
            push_escaped(&mut text, logo);
            format!(
                r##"<text x="32" y="32" text-anchor="middle" dominant-baseline="central" font-family="system-ui, Apple Color Emoji, Segoe UI Emoji, Noto Color Emoji, sans-serif" font-size="{size}" font-weight="600" fill="#e6e9ef">{text}</text>"##
            )
        }
        None => {
            let accent = branding.safe_accent();
            format!(
                r#"<g stroke="{accent}" stroke-width="3.5" stroke-linecap="round"><circle cx="32" cy="32" r="11"/><path d="M32 11v5M32 48v5M11 32h5M48 32h5M17 17l3.5 3.5M43.5 43.5L47 47M17 47l3.5-3.5M43.5 20.5L47 17"/></g>"#
            )
        }
    };
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none"><rect width="64" height="64" rx="14" fill="{panel}"/>{mark}</svg>"#
    )
}

fn typed(mime: &'static str, body: impl IntoResponse) -> Response {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static(mime))],
        body,
    )
        .into_response()
}

pub async fn style_css() -> Response {
    typed("text/css; charset=utf-8", include_str!("static/style.css"))
}

pub async fn app_js() -> Response {
    typed(
        "application/javascript; charset=utf-8",
        include_str!("static/app.js"),
    )
}

pub async fn icon_svg(State(state): State<AppState>) -> Response {
    typed(
        "image/svg+xml; charset=utf-8",
        generated_icon_svg(&state.config().branding),
    )
}

pub async fn manifest(State(state): State<AppState>) -> Response {
    let cfg = state.config();
    let icon_url = brand_icon_url(&cfg.branding);
    let icon = if icon_url == ICON_PATH {
        json!({ "src": icon_url, "sizes": "any", "type": "image/svg+xml", "purpose": "any maskable" })
    } else {
        json!({ "src": icon_url })
    };
    let doc = json!({
        "name": cfg.branding.name,
        "short_name": cfg.branding.name,
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": cfg.branding.safe_background(),
        "theme_color": cfg.branding.safe_accent(),
        "icons": [icon]
    });
    typed("application/manifest+json; charset=utf-8", doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_icon_uses_image_urls_and_escapes_text_logos() {
        let mut branding = BrandingConfig::default();
        for url in ["https://example.com/logo.png", "/custom-logo.svg"] {
            branding.logo = Some(format!(" {url} "));
            assert_eq!(brand_icon_url(&branding), url);
        }
        branding.logo = Some("A<&B".to_owned());
        assert_eq!(brand_icon_url(&branding), ICON_PATH);
        assert!(generated_icon_svg(&branding).contains(">A&lt;&amp;B</text>"));
    }
}
