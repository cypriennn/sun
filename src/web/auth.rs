use crate::{auth::check_origin, domain::UserContext, error::AppError, state::AppState};
use axum::{
    extract::{FromRequestParts, Query, State},
    http::{HeaderMap, HeaderValue, Method, header, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;

impl FromRequestParts<AppState> for UserContext {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cfg = state.config();
        if !cfg.auth.enabled {
            return Ok(Self::local());
        }
        if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            check_origin(&cfg.auth, &parts.headers).map_err(IntoResponse::into_response)?;
        }
        if let Some(user) = state.auth().verify_session(&cfg.auth, &parts.headers).await {
            return Ok(user);
        }
        if parts.uri.path().starts_with("/api/") {
            Err(AppError::Unauthorized("authentication required".to_owned()).into_response())
        } else {
            Err(Redirect::to("/auth/login").into_response())
        }
    }
}

pub async fn login(State(state): State<AppState>) -> Result<Response, AppError> {
    let cfg = state.config();
    if !cfg.auth.enabled {
        return Ok(Redirect::to("/").into_response());
    }
    let (url, cookie) = state.auth().start_login(&cfg.auth, state.secrets()).await?;
    with_cookies(Redirect::to(&url), [cookie])
}

#[derive(Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<Callback>,
) -> Result<Response, AppError> {
    let cfg = state.config();
    if !cfg.auth.enabled {
        return Err(AppError::Unauthorized("OIDC is disabled".to_owned()));
    }
    if params.error.is_some() {
        return Err(AppError::Unauthorized(
            "OIDC authorization denied".to_owned(),
        ));
    }
    let (Some(code), Some(csrf)) = (params.code, params.state) else {
        return Err(AppError::BadRequest(
            "missing OIDC callback parameters".to_owned(),
        ));
    };
    let cookies = state
        .auth()
        .finish_callback(&cfg.auth, state.secrets(), &headers, &code, &csrf)
        .await?;
    with_cookies(Redirect::to("/"), cookies)
}

pub async fn logout(
    State(state): State<AppState>,
    _user: UserContext,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let cookies = state.auth().logout(&state.config().auth, &headers).await;
    with_cookies(Redirect::to("/"), cookies)
}

fn with_cookies(
    response: impl IntoResponse,
    cookies: impl IntoIterator<Item = String>,
) -> Result<Response, AppError> {
    let mut response = response.into_response();
    for cookie in cookies {
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie)
                .map_err(|_| AppError::Internal("invalid cookie".to_owned()))?,
        );
    }
    Ok(response)
}
