//! ChatGPT (Plus/Pro) login — the same OAuth2+PKCE flow the Codex CLI uses
//! (verified against openai/codex Rust source): auth.openai.com, client id
//! app_EMoamEEZ73f0CkXaXp7hrann, loopback on 127.0.0.1:1455 (fallback 1457).

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use url::Url;

use super::jwt_claim;
use super::loopback::Loopback;
use super::pkce;
use crate::store::ChatGptCreds;

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";
pub const SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// Full browser login flow.
pub async fn do_login(http: &reqwest::Client) -> Result<ChatGptCreds> {
    let mut lb = None;
    for port in [1455u16, 1457] {
        match Loopback::bind(Some(port), "/auth/callback").await {
            Ok(l) => {
                lb = Some(l);
                break;
            }
            Err(_) => continue,
        }
    }
    let lb = lb.ok_or_else(|| {
        anyhow!("ports 1455 and 1457 are both busy — close any other login windows and retry")
    })?;
    let redirect_uri = lb.redirect_uri.clone();
    let pk = pkce::generate();
    let state = pkce::random_state();

    let mut url = Url::parse(AUTHORIZE_URL)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", CLIENT_ID);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("code_challenge", &pk.challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", &state);
        q.append_pair("scope", SCOPE);
        q.append_pair("id_token_add_organizations", "true");
        q.append_pair("codex_cli_simplified_flow", "true");
        q.append_pair("originator", "codex_cli_rs");
    }

    println!("Opening browser for ChatGPT sign-in…");
    println!("If it does not open, paste this URL into a browser:\n\n  {url}\n");
    super::open_browser(url.as_str());

    let cb = lb
        .wait_for_callback(Duration::from_secs(300))
        .await
        .context("waiting for ChatGPT login callback")?;
    if cb.state != state {
        bail!("OAuth state mismatch — possible CSRF; aborting login");
    }

    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", &cb.code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &pk.verifier),
        ])
        .send()
        .await
        .context("POST auth.openai.com/oauth/token")?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "token endpoint returned {status}: {}",
            v.get("error_description")
                .or(v.get("error"))
                .map(|e| e.to_string())
                .unwrap_or_default()
        );
    }
    creds_from_response(&v)
}

fn creds_from_response(v: &Value) -> Result<ChatGptCreds> {
    let id_token = v
        .get("id_token")
        .and_then(Value::as_str)
        .context("token response missing id_token")?
        .to_string();
    let access_token = v
        .get("access_token")
        .and_then(Value::as_str)
        .context("token response missing access_token")?
        .to_string();
    let refresh_token = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .context("token response missing refresh_token")?
        .to_string();
    let account_id = jwt_claim(&id_token, "https://api.openai.com/auth.chatgpt_account_id")
        .or_else(|| jwt_claim(&id_token, "chatgpt_account_id"));
    Ok(ChatGptCreds {
        id_token,
        access_token,
        refresh_token,
        account_id,
        last_refresh: Some(rfc3339_now()),
    })
}

/// Refresh (JSON body; refresh tokens ROTATE — persist the new one).
pub async fn refresh(http: &reqwest::Client, c: &ChatGptCreds) -> Result<ChatGptCreds> {
    let resp = http
        .post(TOKEN_URL)
        .json(&json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": c.refresh_token,
        }))
        .send()
        .await
        .context("POST auth.openai.com/oauth/token (refresh)")?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let detail = v
            .get("error_description")
            .or(v.get("error"))
            .map(|e| e.to_string())
            .unwrap_or_default();
        bail!("refresh failed ({status}): {detail} — run: triagent login chatgpt");
    }
    let mut fresh = creds_from_response(&v)?;
    if fresh.id_token.is_empty() {
        fresh.id_token = c.id_token.clone();
    }
    Ok(fresh)
}

/// Best-effort remote revocation of the refresh token.
pub async fn revoke(http: &reqwest::Client, c: &ChatGptCreds) -> Result<()> {
    let resp = http
        .post(REVOKE_URL)
        .json(&json!({
            "token": c.refresh_token,
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        }))
        .send()
        .await?;
    let _ = resp.status();
    Ok(())
}

fn rfc3339_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl ChatGptCreds {
    /// Approximate expiry of the access token from id_token `exp` (seconds).
    pub fn last_refresh_expiry(&self) -> Option<u64> {
        let exp = jwt_claim(&self.id_token, "exp")?.parse::<u64>().ok()?;
        Some(exp * 1000)
    }
}
