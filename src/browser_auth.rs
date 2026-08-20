use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use arboard::Clipboard;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use wreq::{Client, Method};
use wreq_util::Profile;

pub const BROWSER_EXPORTER: &str = include_str!("../browser-console.js");
const PRIVY_SESSION_URL: &str = "https://auth.privy.io/api/v1/sessions";
const PRIVY_CLIENT_ID: &str = "client-WY5gFSayQjxnQhG4rP6SnwPAyPZWZpNRhJ6b9rzMnYwqH";

#[derive(Clone)]
pub struct BrowserAuth {
    pub bearer: String,
    pub expires_at: u64,
    app_id: String,
    refresh_token: Option<String>,
    privy_access_token: Option<String>,
}

impl BrowserAuth {
    pub fn remaining_seconds(&self) -> Result<u64> {
        Ok(self.expires_at.saturating_sub(unix_timestamp()?))
    }

    pub fn can_refresh(&self) -> bool {
        self.refresh_token.is_some()
    }
}

pub fn copy_browser_exporter() -> Result<()> {
    let mut clipboard = Clipboard::new().context("could not open the system clipboard")?;
    clipboard
        .set_text(BROWSER_EXPORTER)
        .context("could not copy the browser helper")
}

pub fn prompt_browser_auth() -> Result<BrowserAuth> {
    let auth_code = rpassword::prompt_password("Paste the browser auth code: ")
        .context("could not read the browser auth code")?;
    let auth = parse_browser_auth(&auth_code)?;
    clear_clipboard_if_unchanged(&auth_code);
    Ok(auth)
}

fn clear_clipboard_if_unchanged(expected: &str) {
    let Ok(mut clipboard) = Clipboard::new() else {
        return;
    };
    if clipboard.get_text().ok().as_deref() == Some(expected) {
        let _ = clipboard.set_text(String::new());
    }
}

pub fn parse_browser_auth(auth: &str) -> Result<BrowserAuth> {
    let value: serde_json::Value = serde_json::from_str(auth.trim()).context(
        "clipboard does not contain FOMO auth; run the helper in the FOMO console, wait for \"fresh auth copied\", then try again",
    )?;
    if !matches!(value.get("version").and_then(Value::as_u64), Some(1 | 2)) {
        bail!("browser auth code has an unsupported version");
    }
    let bearer = value
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("browser auth code did not contain an access token"))?;
    let token = validate_access_token(bearer)?;
    let now = unix_timestamp()?;
    if token.expires_at <= now {
        bail!(
            "JWT already expired {} ago; refresh FOMO and run the browser helper again",
            format_duration(now - token.expires_at)
        );
    }

    Ok(BrowserAuth {
        bearer: bearer.to_owned(),
        expires_at: token.expires_at,
        app_id: token.app_id,
        refresh_token: optional_credential(&value, "refreshToken"),
        privy_access_token: optional_credential(&value, "privyAccessToken"),
    })
}

pub async fn refresh_browser_auth(auth: &BrowserAuth) -> Result<BrowserAuth> {
    let refresh_token = auth
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("browser auth did not include refresh support"))?;
    let now = unix_timestamp()?;
    let authorization = auth
        .privy_access_token
        .as_deref()
        .filter(|token| jwt_expiration(token).is_none_or(|expires| expires > now + 60))
        .unwrap_or(&auth.bearer);
    let client = Client::builder()
        .emulation(Profile::Chrome148)
        .timeout(Duration::from_secs(30))
        .build()
        .context("could not build the Privy refresh client")?;
    let response = client
        .request(Method::POST, PRIVY_SESSION_URL)
        .bearer_auth(authorization)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("origin", "https://fomo.family")
        .header("referer", "https://fomo.family/")
        .header("privy-app-id", &auth.app_id)
        .header("privy-client-id", PRIVY_CLIENT_ID)
        .header("privy-client", "react-auth:3.21.3")
        .header("sec-fetch-dest", "empty")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "cross-site")
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .context("Privy session refresh failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Privy session refresh returned HTTP {status}");
    }
    let value: Value = response
        .json()
        .await
        .context("Privy session refresh returned invalid JSON")?;

    let bearer = optional_credential(&value, "token").unwrap_or_else(|| auth.bearer.clone());
    let token = validate_access_token(&bearer)?;
    if token.app_id != auth.app_id {
        bail!("Privy session refresh returned a token for a different application");
    }
    if token.expires_at <= now + 30 {
        bail!("Privy session refresh did not return a usable access token");
    }

    Ok(BrowserAuth {
        bearer,
        expires_at: token.expires_at,
        app_id: token.app_id,
        refresh_token: optional_credential(&value, "refresh_token")
            .or_else(|| auth.refresh_token.clone()),
        privy_access_token: optional_credential(&value, "privy_access_token")
            .or_else(|| auth.privy_access_token.clone()),
    })
}

struct AccessToken {
    app_id: String,
    expires_at: u64,
}

fn validate_access_token(bearer: &str) -> Result<AccessToken> {
    if bearer.split('.').count() != 3 {
        bail!("browser auth code did not contain a JWT access token");
    }
    let encoded_claims = bearer
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("JWT did not contain claims"))?;
    let claims_bytes = URL_SAFE_NO_PAD
        .decode(encoded_claims)
        .context("JWT claims were not valid base64url")?;
    let claims: Value =
        serde_json::from_slice(&claims_bytes).context("JWT claims were not valid JSON")?;
    if claims.get("iss").and_then(Value::as_str) != Some("privy.io") {
        bail!("JWT was not issued by Privy");
    }
    let app_id = claims
        .get("aud")
        .and_then(Value::as_str)
        .filter(|audience| !audience.is_empty())
        .ok_or_else(|| anyhow!("JWT did not contain a Privy application audience"))?;
    let expires_at = claims
        .get("exp")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("JWT did not contain a valid expiration"))?;
    Ok(AccessToken {
        app_id: app_id.to_owned(),
        expires_at,
    })
}

fn jwt_expiration(token: &str) -> Option<u64> {
    validate_access_token(token)
        .ok()
        .map(|token| token.expires_at)
}

fn optional_credential(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

pub fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    use super::{parse_browser_auth, unix_timestamp};

    fn auth_code(issuer: &str, audience: &str, expires_at: u64) -> (String, String) {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"ES256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iss": issuer,
                "aud": audience,
                "exp": expires_at,
            })
            .to_string(),
        );
        let token = format!("{header}.{claims}.signature");
        let code = serde_json::json!({ "version": 1, "accessToken": token }).to_string();
        (code, token)
    }

    #[test]
    fn parses_browser_auth_code() {
        let (auth, token) = auth_code("privy.io", "fomo-app", 4_000_000_000);
        let parsed = parse_browser_auth(&auth).unwrap();

        assert_eq!(parsed.bearer, token);
        assert_eq!(parsed.expires_at, 4_000_000_000);
        assert!(!parsed.can_refresh());
    }

    #[test]
    fn parses_memory_only_refresh_material() {
        let (auth, _) = auth_code("privy.io", "fomo-app", 4_000_000_000);
        let mut value: serde_json::Value = serde_json::from_str(&auth).unwrap();
        value["version"] = serde_json::json!(2);
        value["refreshToken"] = serde_json::json!("opaque-refresh-token");
        value["privyAccessToken"] = serde_json::json!("opaque-privy-access-token");

        let parsed = parse_browser_auth(&value.to_string()).unwrap();

        assert!(parsed.can_refresh());
        assert_eq!(
            parsed.refresh_token.as_deref(),
            Some("opaque-refresh-token")
        );
        assert_eq!(
            parsed.privy_access_token.as_deref(),
            Some("opaque-privy-access-token")
        );
    }

    #[test]
    fn rejects_refresh_token_shaped_input() {
        let auth = r#"{"version":1,"accessToken":"opaque-refresh-token"}"#;

        assert!(parse_browser_auth(auth).is_err());
    }

    #[test]
    fn explains_when_the_helper_was_pasted_back() {
        let error = parse_browser_auth("(() => console.log('helper'))()")
            .err()
            .unwrap()
            .to_string();

        assert!(error.contains("run the helper in the FOMO console"));
        assert!(error.contains("fresh auth copied"));
    }

    #[test]
    fn rejects_missing_privy_app_audience() {
        let (auth, _) = auth_code("privy.io", "", 4_000_000_000);

        assert!(parse_browser_auth(&auth).is_err());
    }

    #[test]
    fn rejects_expired_token() {
        let (auth, _) = auth_code("privy.io", "fomo-app", unix_timestamp().unwrap() - 1);

        assert!(parse_browser_auth(&auth).is_err());
    }
}
