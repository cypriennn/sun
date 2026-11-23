use crate::{
    config::{AuthConfig, Secrets},
    domain::UserContext,
    error::AppError,
    storage::now_millis,
};
use axum::http::{HeaderMap, header};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use openidconnect::{
    AccessTokenHash, AdditionalClaims, AsyncHttpClient, AuthenticationFlow, AuthorizationCode,
    Client, ClientId, ClientSecret, CsrfToken, EmptyExtraTokenFields, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, HttpRequest, HttpResponse, IdTokenFields, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    StandardErrorResponse, StandardTokenResponse, TokenResponse,
    core::{
        CoreAuthDisplay, CoreAuthPrompt, CoreErrorResponseType, CoreGenderClaim, CoreJsonWebKey,
        CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm, CoreProviderMetadata,
        CoreResponseType, CoreRevocableToken, CoreRevocationErrorResponse,
        CoreTokenIntrospectionResponse, CoreTokenType,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, collections::HashMap, future::Future, pin::Pin, time::Duration};
use tokio::sync::Mutex;

const SESSION_COOKIE: &str = "sun_session";
const STATE_COOKIE: &str = "sun_oidc_state";
const LOGIN_TTL: u64 = 600;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExtraClaims {
    #[serde(flatten)]
    pub values: BTreeMap<String, Value>,
}

impl AdditionalClaims for ExtraClaims {}

type OidcTokenResponse = StandardTokenResponse<
    IdTokenFields<
        ExtraClaims,
        EmptyExtraTokenFields,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    >,
    CoreTokenType,
>;

type OidcClient = Client<
    ExtraClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    OidcTokenResponse,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

#[derive(Clone)]
struct OidcHttpClient(reqwest::Client);

impl<'c> AsyncHttpClient<'c> for OidcHttpClient {
    type Error = AppError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + 'c>>;

    fn call(&'c self, req: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let url = openidconnect::url::Url::parse(&parts.uri.to_string())
                .map_err(|_| AppError::Upstream("invalid OIDC endpoint URL".to_owned()))?;
            if !secure_url(&url) {
                return Err(AppError::Upstream(
                    "insecure OIDC endpoint rejected".to_owned(),
                ));
            }
            let mut response = self
                .0
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body)
                .send()
                .await?;
            let mut builder = openidconnect::http::Response::builder().status(response.status());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
                    return Err(AppError::Upstream("OIDC response is too large".to_owned()));
                }
                bytes.extend_from_slice(&chunk);
            }
            builder
                .body(bytes)
                .map_err(|e| AppError::Internal(e.to_string()))
        })
    }
}

struct PendingLogin {
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    binding: [u8; 32],
    expires: u64,
}

struct Session {
    user: UserContext,
    binding: [u8; 32],
    expires: u64,
}

pub struct AuthManager {
    http: OidcHttpClient,
    metadata: Mutex<Option<(String, u64, CoreProviderMetadata)>>,
    logins: Mutex<HashMap<String, PendingLogin>>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl AuthManager {
    pub fn new() -> Result<Self, AppError> {
        Ok(Self {
            http: OidcHttpClient(
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(15))
                    .build()?,
            ),
            metadata: Mutex::new(None),
            logins: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub async fn invalidate(&self) {
        self.logins.lock().await.clear();
        self.sessions.lock().await.clear();
        *self.metadata.lock().await = None;
    }

    async fn client(&self, cfg: &AuthConfig, secrets: &Secrets) -> Result<OidcClient, AppError> {
        let issuer = cfg.issuer_url.as_deref().unwrap_or_default();
        let mut cache = self.metadata.lock().await;
        if !cache
            .as_ref()
            .is_some_and(|(url, exp, _)| url == issuer && *exp > now_seconds())
        {
            let issuer_url = IssuerUrl::new(issuer.to_owned())
                .map_err(|_| AppError::BadRequest("invalid OIDC issuer URL".to_owned()))?;
            let metadata = CoreProviderMetadata::discover_async(issuer_url, &self.http)
                .await
                .map_err(|_| AppError::Upstream("OIDC discovery failed".to_owned()))?;
            for endpoint in [
                Some(metadata.authorization_endpoint().url()),
                metadata.token_endpoint().map(|u| u.url()),
                metadata.userinfo_endpoint().map(|u| u.url()),
                Some(metadata.jwks_uri().url()),
            ]
            .into_iter()
            .flatten()
            {
                if !secure_url(endpoint) {
                    return Err(AppError::Upstream(
                        "OIDC endpoints must use HTTPS (HTTP is allowed only on loopback)"
                            .to_owned(),
                    ));
                }
            }
            *cache = Some((issuer.to_owned(), now_seconds() + 300, metadata));
        }
        let metadata = cache.as_ref().expect("metadata loaded").2.clone();
        let redirect = RedirectUrl::new(cfg.redirect_url.clone().unwrap_or_default())
            .map_err(|_| AppError::BadRequest("invalid OIDC redirect URL".to_owned()))?;
        Ok(OidcClient::from_provider_metadata(
            metadata,
            ClientId::new(cfg.client_id.clone().unwrap_or_default()),
            secrets.oidc_client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(redirect))
    }

    pub async fn start_login(
        &self,
        cfg: &AuthConfig,
        secrets: &Secrets,
    ) -> Result<(String, String), AppError> {
        let client = self.client(cfg, secrets).await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = client
            .authorize_url(
                AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .set_pkce_challenge(challenge);
        for scope in &cfg.scopes {
            if !scope.trim().is_empty() && scope != "openid" {
                request = request.add_scope(Scope::new(scope.clone()));
            }
        }
        let (url, state, nonce) = request.url();
        let mut logins = self.logins.lock().await;
        logins.retain(|_, p| p.expires > now_seconds());
        if logins.len() >= 4096 {
            return Err(AppError::TooManyRequests(
                "too many pending logins".to_owned(),
            ));
        }
        logins.insert(
            state.secret().clone(),
            PendingLogin {
                verifier,
                nonce,
                binding: binding(cfg),
                expires: now_seconds() + LOGIN_TTL,
            },
        );
        Ok((
            url.to_string(),
            cookie(cfg, STATE_COOKIE, state.secret(), LOGIN_TTL),
        ))
    }

    pub async fn finish_callback(
        &self,
        cfg: &AuthConfig,
        secrets: &Secrets,
        headers: &HeaderMap,
        code: &str,
        state: &str,
    ) -> Result<Vec<String>, AppError> {
        let supplied = extract_cookie(headers, STATE_COOKIE).unwrap_or_default();
        if supplied.is_empty()
            || Sha256::digest(supplied.as_bytes()) != Sha256::digest(state.as_bytes())
        {
            return Err(AppError::Unauthorized("OIDC state mismatch".to_owned()));
        }
        let pending = self
            .logins
            .lock()
            .await
            .remove(state)
            .filter(|p| p.expires > now_seconds() && p.binding == binding(cfg))
            .ok_or_else(|| {
                AppError::Unauthorized("OIDC login expired; please sign in again".to_owned())
            })?;
        let client = self.client(cfg, secrets).await?;
        let response = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|_| AppError::Upstream("missing OIDC token endpoint".to_owned()))?
            .set_pkce_verifier(pending.verifier)
            .request_async(&self.http)
            .await
            .map_err(|_| AppError::Unauthorized("OIDC code exchange failed".to_owned()))?;
        let token = response.id_token().ok_or_else(|| {
            AppError::Unauthorized("provider did not return an ID token".to_owned())
        })?;
        let verifier = client.id_token_verifier();
        let claims = token
            .claims(&verifier, &pending.nonce)
            .map_err(|_| AppError::Unauthorized("invalid OIDC ID token".to_owned()))?;
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                response.access_token(),
                token
                    .signing_alg()
                    .map_err(|_| AppError::Unauthorized("invalid signing algorithm".to_owned()))?,
                token
                    .signing_key(&verifier)
                    .map_err(|_| AppError::Unauthorized("invalid signing key".to_owned()))?,
            )
            .map_err(|_| AppError::Unauthorized("invalid access token".to_owned()))?;
            if actual != *expected {
                return Err(AppError::Unauthorized(
                    "access token hash mismatch".to_owned(),
                ));
            }
        }
        let subject = claims.subject().clone();
        let expires = u64::try_from(claims.expiration().timestamp())
            .unwrap_or(0)
            .min(now_seconds().saturating_add(cfg.session_ttl_seconds));
        if expires <= now_seconds() {
            return Err(AppError::Unauthorized("ID token expired".to_owned()));
        }
        let mut groups = extract_groups(&claims.additional_claims().values, &cfg.groups_claim);
        if groups.is_none()
            && let Ok(request) =
                client.user_info(response.access_token().clone(), Some(subject.clone()))
        {
            let info = request
                .request_async::<ExtraClaims, _, CoreGenderClaim>(&self.http)
                .await
                .map_err(|_| {
                    AppError::Unauthorized("OIDC userinfo verification failed".to_owned())
                })?;
            groups = extract_groups(&info.additional_claims().values, &cfg.groups_claim);
        }
        let user = UserContext {
            user_id: derive_user_id(
                cfg.issuer_url.as_deref().unwrap_or_default(),
                subject.as_str(),
            ),
            groups: groups.unwrap_or_default(),
        };
        let sid = CsrfToken::new_random().secret().clone();
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, s| s.expires > now_seconds());
        if sessions.len() >= 16_384 {
            return Err(AppError::TooManyRequests("too many sessions".to_owned()));
        }
        if let Some(old) = extract_cookie(headers, SESSION_COOKIE) {
            sessions.remove(old);
        }
        sessions.insert(
            sid.clone(),
            Session {
                user,
                expires,
                binding: binding(cfg),
            },
        );
        Ok(vec![
            cookie(
                cfg,
                SESSION_COOKIE,
                &sid,
                expires.saturating_sub(now_seconds()),
            ),
            cookie(cfg, STATE_COOKIE, "", 0),
        ])
    }

    pub async fn verify_session(
        &self,
        cfg: &AuthConfig,
        headers: &HeaderMap,
    ) -> Option<UserContext> {
        let sid = extract_cookie(headers, SESSION_COOKIE)?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions.get(sid)?;
        if session.expires <= now_seconds() || session.binding != binding(cfg) {
            sessions.remove(sid);
            return None;
        }
        Some(session.user.clone())
    }

    pub async fn active_user(&self, cfg: &AuthConfig, user_id: &str) -> Option<UserContext> {
        let binding = binding(cfg);
        let now = now_seconds();
        let sessions = self.sessions.lock().await;
        let mut matching = sessions
            .values()
            .filter(|s| s.user.user_id == user_id && s.binding == binding && s.expires > now);
        let user = &matching.next()?.user;
        // Do not choose between sessions with conflicting group claims.
        matching
            .all(|s| s.user.groups == user.groups)
            .then(|| user.clone())
    }

    pub async fn logout(&self, cfg: &AuthConfig, headers: &HeaderMap) -> Vec<String> {
        if let Some(sid) = extract_cookie(headers, SESSION_COOKIE) {
            self.sessions.lock().await.remove(sid);
        }
        vec![
            cookie(cfg, SESSION_COOKIE, "", 0),
            cookie(cfg, STATE_COOKIE, "", 0),
        ]
    }
}

pub fn derive_user_id(issuer: &str, subject: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(issuer.as_bytes());
    hash.update([0]);
    hash.update(subject.as_bytes());
    format!("u_{}", URL_SAFE_NO_PAD.encode(hash.finalize()))
}

fn binding(cfg: &AuthConfig) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(cfg).expect("serializable auth config")).into()
}

fn extract_groups(map: &BTreeMap<String, Value>, key: &str) -> Option<Vec<String>> {
    let object = serde_json::to_value(map).ok()?;
    let value = if key.starts_with('/') {
        object.pointer(key)?
    } else {
        object.get(key)?
    };
    let mut groups: Vec<String> = match value {
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Value::String(s) => s.split([',', ' ']).map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    groups.retain(|g| !g.is_empty());
    groups.sort();
    groups.dedup();
    Some(groups)
}

pub fn secure_url(url: &openidconnect::url::Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && (url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))))
}

pub fn check_origin(cfg: &AuthConfig, headers: &HeaderMap) -> Result<(), AppError> {
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return Err(AppError::Unauthorized(
            "cross-site request rejected".to_owned(),
        ));
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let expected = cfg
            .redirect_url
            .as_deref()
            .and_then(|u| openidconnect::url::Url::parse(u).ok())
            .map(|u| u.origin().ascii_serialization());
        if expected.as_deref() != origin.to_str().ok() {
            return Err(AppError::Unauthorized("invalid request origin".to_owned()));
        }
    }
    Ok(())
}

fn cookie(cfg: &AuthConfig, name: &str, value: &str, ttl: u64) -> String {
    let secure = if cfg
        .redirect_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"))
    {
        "; Secure"
    } else {
        ""
    };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{secure}")
}

fn extract_cookie<'a>(headers: &'a HeaderMap, target: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|s| s.trim().split_once('='))
        .find_map(|(name, value)| (name == target).then_some(value))
}

fn now_seconds() -> u64 {
    now_millis() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opaque_sessions_expire_and_do_not_survive_restart_or_logout() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = AuthConfig {
            enabled: true,
            redirect_url: Some("https://sun.example.com/auth/callback".to_owned()),
            ..Default::default()
        };
        let auth = AuthManager::new().expect("auth");
        let sid = CsrfToken::new_random().secret().clone();
        let headers = HeaderMap::from_iter([(
            header::COOKIE,
            format!("sun_session={sid}").parse().expect("cookie"),
        )]);
        auth.sessions.lock().await.insert(
            sid.clone(),
            Session {
                user: UserContext {
                    user_id: derive_user_id("issuer", "subject"),
                    groups: vec!["sre".to_owned()],
                },
                binding: binding(&cfg),
                expires: now_seconds() + 60,
            },
        );
        assert!(auth.verify_session(&cfg, &headers).await.is_some());
        assert!(
            AuthManager::new()
                .expect("auth")
                .verify_session(&cfg, &headers)
                .await
                .is_none()
        );
        let cookie = cookie(&cfg, SESSION_COOKIE, &sid, 60);
        assert!(
            cookie.contains("; Secure")
                && cookie.contains("HttpOnly")
                && !cookie.contains("Domain=")
        );
        auth.sessions
            .lock()
            .await
            .get_mut(&sid)
            .expect("session")
            .expires = now_seconds();
        assert!(auth.verify_session(&cfg, &headers).await.is_none());
        assert!(auth.sessions.lock().await.is_empty());
        assert_eq!(
            extract_groups(
                &BTreeMap::from([("realm_access".to_owned(), json_value())]),
                "/realm_access/roles"
            ),
            Some(vec!["sre".to_owned()])
        );
    }

    fn json_value() -> Value {
        serde_json::json!({"roles": ["sre", "sre"]})
    }
}
