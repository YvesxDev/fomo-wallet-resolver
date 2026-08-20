use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, Uri, header, uri::Authority},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use fomo_wallet_resolver::{
    DEFAULT_RPC_URL, EvmResolution, FomoWalletResolver, ResolverConfig, parse_solana_wallet,
    resolve_solana_wallet,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::RwLock,
    time::{Duration, MissedTickBehavior, interval},
};

use crate::browser_auth::{
    BROWSER_EXPORTER, BrowserAuth, parse_browser_auth, refresh_browser_auth, unix_timestamp,
};

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const CONSOLE_GUIDE: &[u8] = include_bytes!("../ui/console-guide.png");
const REFRESH_SKEW_SECONDS: u64 = 5 * 60;
const REFRESH_RETRY_SECONDS: u64 = 30;

#[derive(Default)]
struct SessionState {
    auth: Option<BrowserAuth>,
    generation: u64,
    auto_refresh: bool,
    refreshing: bool,
    next_refresh_attempt_at: u64,
    refresh_error: Option<String>,
    last_refresh_at: Option<u64>,
    last_refresh_extended: Option<bool>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            auto_refresh: true,
            ..Self::default()
        }
    }

    fn disconnect(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.auth = None;
        self.refreshing = false;
        self.next_refresh_attempt_at = 0;
        self.refresh_error = None;
        self.last_refresh_at = None;
        self.last_refresh_extended = None;
    }

    fn disconnect_if_current(&mut self, bearer: &str) {
        if self
            .auth
            .as_ref()
            .is_some_and(|active| active.bearer == bearer)
        {
            self.disconnect();
        }
    }
}

#[derive(Clone)]
struct AppState {
    session: Arc<RwLock<SessionState>>,
}

pub async fn run() -> Result<()> {
    let state = AppState {
        session: Arc::new(RwLock::new(SessionState::new())),
    };
    tokio::spawn(auto_refresh_loop(state.clone()));
    let app = router(state);

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("could not start the local interface")?;
    let address = listener.local_addr()?;
    let url = format!("http://{address}");
    println!("FOMO Resolver UI: {url}");
    println!("Keep this terminal open. Press Ctrl+C to stop.");
    if let Err(error) = webbrowser::open(&url) {
        eprintln!("Could not open the browser automatically: {error}");
        eprintln!("Open this address manually: {url}");
    }

    axum::serve(listener, app)
        .await
        .context("local interface stopped unexpectedly")
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(styles))
        .route("/app.js", get(script))
        .route("/console-guide.png", get(console_guide))
        .route("/browser-console.js", get(browser_exporter))
        .route("/api/auth", post(authenticate))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/refresh", post(request_auth_refresh))
        .route("/api/auth/auto-refresh", post(set_auto_refresh))
        .route("/api/resolve", post(resolve))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn styles() -> Response {
    asset(APP_CSS, "text/css; charset=utf-8")
}

async fn script() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}

async fn console_guide() -> Response {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
        CONSOLE_GUIDE,
    )
        .into_response()
}

async fn browser_exporter() -> Response {
    asset(BROWSER_EXPORTER, "text/javascript; charset=utf-8")
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body,
    )
        .into_response()
}

async fn security_headers(request: Request, next: Next) -> Response {
    if !has_loopback_host(&request) || !has_matching_origin(&request) {
        return (StatusCode::FORBIDDEN, "Local requests only").into_response();
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn has_loopback_host(request: &Request) -> bool {
    request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok())
        .is_some_and(|authority| {
            authority.host() == "127.0.0.1" || authority.host().eq_ignore_ascii_case("localhost")
        })
}

fn has_matching_origin(request: &Request) -> bool {
    let Some(origin) = request.headers().get(header::ORIGIN) else {
        return true;
    };
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origin
        .to_str()
        .ok()
        .and_then(|value| value.parse::<Uri>().ok())
        .is_some_and(|origin| {
            origin.scheme_str() == Some("http")
                && origin
                    .authority()
                    .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(host))
        })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthRequest {
    auth_code: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatusResponse {
    connected: bool,
    expires_at: Option<u64>,
    auto_refresh: bool,
    can_refresh: bool,
    refreshing: bool,
    refresh_error: Option<String>,
    last_refresh_at: Option<u64>,
    last_refresh_extended: Option<bool>,
}

async fn authenticate(
    State(state): State<AppState>,
    Json(request): Json<AuthRequest>,
) -> ApiResult<AuthStatusResponse> {
    let auth = parse_browser_auth(&request.auth_code).map_err(ApiError::bad_request)?;
    let now = unix_timestamp().map_err(ApiError::internal)?;
    let mut session = state.session.write().await;
    session.generation = session.generation.wrapping_add(1);
    session.auth = Some(auth);
    session.refreshing = false;
    session.refresh_error = None;
    session.next_refresh_attempt_at = 0;
    session.last_refresh_at = None;
    session.last_refresh_extended = None;
    Ok(Json(auth_status_response(&session, now)))
}

async fn auth_status(State(state): State<AppState>) -> ApiResult<AuthStatusResponse> {
    let now = unix_timestamp().map_err(ApiError::internal)?;
    let session = state.session.read().await;
    Ok(Json(auth_status_response(&session, now)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutoRefreshRequest {
    enabled: bool,
}

async fn set_auto_refresh(
    State(state): State<AppState>,
    Json(request): Json<AutoRefreshRequest>,
) -> ApiResult<AuthStatusResponse> {
    let now = unix_timestamp().map_err(ApiError::internal)?;
    let mut session = state.session.write().await;
    session.auto_refresh = request.enabled;
    Ok(Json(auth_status_response(&session, now)))
}

async fn request_auth_refresh(State(state): State<AppState>) -> ApiResult<AuthStatusResponse> {
    refresh_auth(&state, true).await.map(Json)
}

fn auth_status_response(session: &SessionState, now: u64) -> AuthStatusResponse {
    let expires_at = session.auth.as_ref().map(|auth| auth.expires_at);
    let remaining_seconds = expires_at.map_or(0, |expires| expires.saturating_sub(now));
    AuthStatusResponse {
        connected: remaining_seconds > 30,
        expires_at,
        auto_refresh: session.auto_refresh,
        can_refresh: session.auth.as_ref().is_some_and(BrowserAuth::can_refresh),
        refreshing: session.refreshing,
        refresh_error: session.refresh_error.clone(),
        last_refresh_at: session.last_refresh_at,
        last_refresh_extended: session.last_refresh_extended,
    }
}

async fn auto_refresh_loop(state: AppState) {
    let mut timer = interval(Duration::from_secs(15));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        let _ = refresh_auth(&state, false).await;
    }
}

async fn refresh_auth(state: &AppState, force: bool) -> Result<AuthStatusResponse, ApiError> {
    let now = unix_timestamp().map_err(ApiError::internal)?;
    let (current, generation) = {
        let mut session = state.session.write().await;
        let Some(auth) = session.auth.clone() else {
            if force {
                return Err(ApiError::bad_request("Connect your FOMO session first"));
            }
            return Ok(auth_status_response(&session, now));
        };
        if !auth.can_refresh() {
            if force {
                return Err(ApiError::bad_request(
                    "Copy and run the latest browser helper to enable refresh",
                ));
            }
            return Ok(auth_status_response(&session, now));
        }
        let due = auth.expires_at <= now.saturating_add(REFRESH_SKEW_SECONDS);
        if session.refreshing
            || (!force && !session.auto_refresh)
            || (!force && !due)
            || (!force && now < session.next_refresh_attempt_at)
        {
            return Ok(auth_status_response(&session, now));
        }
        session.refreshing = true;
        session.refresh_error = None;
        (auth, session.generation)
    };

    let previous_expiry = current.expires_at;
    let refreshed = refresh_browser_auth(&current).await;
    let finished_at = unix_timestamp().map_err(ApiError::internal)?;
    let mut session = state.session.write().await;
    if session.generation != generation {
        return Ok(auth_status_response(&session, finished_at));
    }
    session.refreshing = false;
    match refreshed {
        Ok(auth) => {
            let next_expiry = auth.expires_at;
            if session.auth.as_ref().is_some_and(|active| {
                active.expires_at == previous_expiry && active.bearer == current.bearer
            }) {
                session.auth = Some(auth);
            }
            session.next_refresh_attempt_at = if next_expiry > previous_expiry {
                0
            } else {
                finished_at.saturating_add(REFRESH_RETRY_SECONDS)
            };
            session.refresh_error = None;
            session.last_refresh_at = Some(finished_at);
            session.last_refresh_extended = Some(next_expiry > previous_expiry);
            Ok(auth_status_response(&session, finished_at))
        }
        Err(error) => {
            session.next_refresh_attempt_at = finished_at.saturating_add(REFRESH_RETRY_SECONDS);
            session.refresh_error = Some(error.to_string());
            session.last_refresh_at = Some(finished_at);
            session.last_refresh_extended = None;
            Err(ApiError::upstream(format!(
                "Could not refresh FOMO authentication: {error}"
            )))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveRequest {
    input: String,
    rpc_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveResponse {
    solana_wallet: String,
    evm_wallet: Option<String>,
    evm_unavailable_reason: Option<&'static str>,
    direct_wallet_input: bool,
    timings: TimingsResponse,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TimingsResponse {
    profile_ms: u64,
    builder_ms: u64,
    decode_ms: u64,
    rpc_ms: u64,
    evm_ms: u64,
    total_ms: u64,
}

async fn resolve(
    State(state): State<AppState>,
    Json(request): Json<ResolveRequest>,
) -> ApiResult<ResolveResponse> {
    let input = request.input.trim();
    if input.is_empty() {
        return Err(ApiError::bad_request(
            "Enter a Solana wallet, FOMO handle, or profile URL",
        ));
    }

    let direct_wallet_input = parse_solana_wallet(input).is_some();
    let result = if direct_wallet_input {
        resolve_solana_wallet(input)
            .await
            .map_err(|error| ApiError::upstream(error.to_string()))?
    } else {
        let now = unix_timestamp().map_err(ApiError::internal)?;
        let auth = {
            let mut session = state.session.write().await;
            let auth = session
                .auth
                .clone()
                .ok_or_else(|| ApiError::unauthorized("Connect your FOMO session first"))?;
            if auth.expires_at <= now.saturating_add(30) {
                session.disconnect();
                return Err(ApiError::unauthorized(
                    "FOMO authentication expired. Run the browser helper again.",
                ));
            }
            auth
        };

        let rpc_url = match request.rpc_url.trim() {
            "" => DEFAULT_RPC_URL.to_owned(),
            value => value.to_owned(),
        };
        let config = ResolverConfig::with_rpc(auth.bearer.clone(), rpc_url)
            .map_err(ApiError::bad_request)?;
        let resolver = FomoWalletResolver::new(config).map_err(ApiError::internal)?;
        match resolver.resolve(input).await {
            Ok(result) => result,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("401 Unauthorized")
                    || message.contains("JWT authentication middleware")
                {
                    state
                        .session
                        .write()
                        .await
                        .disconnect_if_current(&auth.bearer);
                    return Err(ApiError::unauthorized(
                        "FOMO rejected the authentication. Run the browser helper again.",
                    ));
                }
                return Err(ApiError::upstream(message));
            }
        }
    };

    let (evm_wallet, evm_unavailable_reason) = match result.evm {
        EvmResolution::Resolved(wallet) => (Some(wallet), None),
        EvmResolution::Unavailable(reason) => (None, Some(reason.message())),
    };
    Ok(Json(ResolveResponse {
        solana_wallet: result.solana_wallet.to_string(),
        evm_wallet,
        evm_unavailable_reason,
        direct_wallet_input,
        timings: TimingsResponse {
            profile_ms: millis(result.timings.profile),
            builder_ms: millis(result.timings.builder),
            decode_ms: millis(result.timings.decode),
            rpc_ms: millis(result.timings.rpc),
            evm_ms: millis(result.timings.evm),
            total_ms: millis(result.timings.total),
        },
    }))
}

fn millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

type ApiResult<T> = Result<Json<T>, ApiError>;

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }

        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    fn test_state() -> AppState {
        AppState {
            session: Arc::new(RwLock::new(SessionState::new())),
        }
    }

    fn auth_code(audience: &str, expires_at: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"ES256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            json!({
                "iss": "privy.io",
                "aud": audience,
                "exp": expires_at,
            })
            .to_string(),
        );
        json!({
            "version": 2,
            "accessToken": format!("{header}.{claims}.signature"),
            "refreshToken": "secret-refresh",
            "privyAccessToken": "secret-privy-access",
        })
        .to_string()
    }

    async fn response_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn auth_routes_never_return_credentials() {
        let app = router(test_state());
        let body = json!({
            "authCode": auth_code("fomo-app", 4_000_000_000),
        })
        .to_string();
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/auth")
                    .header(header::HOST, "127.0.0.1:1234")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;
        let serialized = payload.to_string();
        assert_eq!(payload["connected"], true);
        assert!(!serialized.contains("secret-refresh"));
        assert!(!serialized.contains("secret-privy-access"));
        assert!(payload.get("accessToken").is_none());

        let response = app
            .oneshot(
                Request::get("/api/auth/status")
                    .header(header::HOST, "127.0.0.1:1234")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response_json(response)
                .await
                .to_string()
                .contains("secret-")
        );
    }

    #[tokio::test]
    async fn resolve_requires_authentication() {
        let response = router(test_state())
            .oneshot(
                Request::post("/api/resolve")
                    .header(header::HOST, "127.0.0.1:1234")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"input": "example", "rpcUrl": ""}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response_json(response).await["error"],
            "Connect your FOMO session first"
        );
    }

    #[tokio::test]
    async fn serves_the_console_guide() {
        let response = router(test_state())
            .oneshot(
                Request::get("/console-guide.png")
                    .header(header::HOST, "127.0.0.1:1234")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn stale_rejection_does_not_clear_replacement_auth() {
        let old = parse_browser_auth(&auth_code("old-app", 4_000_000_000)).unwrap();
        let replacement = parse_browser_auth(&auth_code("new-app", 4_000_000_000)).unwrap();
        let mut session = SessionState::new();
        session.auth = Some(replacement.clone());

        session.disconnect_if_current(&old.bearer);

        assert_eq!(session.auth.unwrap().bearer, replacement.bearer);
    }

    #[tokio::test]
    async fn rejects_non_loopback_hosts() {
        let response = router(test_state())
            .oneshot(
                Request::get("/")
                    .header(header::HOST, "attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn rejects_cross_origin_requests() {
        let response = router(test_state())
            .oneshot(
                Request::post("/api/auth/status")
                    .header(header::HOST, "127.0.0.1:1234")
                    .header(header::ORIGIN, "https://attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
