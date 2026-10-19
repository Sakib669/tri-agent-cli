//! claude.ai subscription OAuth — flow verified against Claude Code 2.1.302:
//! authorize at claude.com/cai/oauth/authorize, JSON token exchange at
//! platform.claude.com/v1/oauth/token, inference via api.anthropic.com with
//! `anthropic-beta: oauth-2025-04-20`.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use url::Url;

use super::loopback::Loopback;
use super::now_ms;
use super::pkce;
use crate::store::ClaudeCreds;

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Full browser login flow.
pub async fn do_login(http: &reqwest::Client) -> Result<ClaudeCreds> {
    let lb = Loopback::bind(None, "/callback").await?;
    let redirect_uri = lb.redirect_uri.clone();
    let pk = pkce::generate();
    let state = pkce::random_state();

    let mut url = Url::parse(AUTHORIZE_URL)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("code", "true");
        q.append_pair("client_id", CLIENT_ID);
        q.append_pair("response_type", "code");
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("scope", SCOPES);
        q.append_pair("code_challenge", &pk.challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", &state);
    }

    println!("Opening browser for claude.ai sign-in…");
    println!("If it does not open, paste this URL into a browser:\n\n  {url}\n");
    super::open_browser(url.as_str());

    let cb = lb
        .wait_for_callback(Duration::from_secs(300))
        .await
        .context("waiting for claude.ai login callback")?;
    if cb.state != state {
        bail!("OAuth state mismatch — possible CSRF; aborting login");
    }
    let resp = exchange(
        http,
        json!({
            "grant_type": "authorization_code",
            "code": cb.code,
            "redirect_uri": redirect_uri,
            "client_id": CLIENT_ID,
            "code_verifier": pk.verifier,
            "state": state,
        }),
    )
    .await?;
    creds_from_response(&resp)
}

async fn exchange(http: &reqwest::Client, body: Value) -> Result<Value> {
    let resp = http
        .post(TOKEN_URL)
        .json(&body)
        .send()
        .await
        .context("POST platform.claude.com/v1/oauth/token")?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("token endpoint returned {status}: {}", truncate(&text, 400));
    }
    serde_json::from_str(&text).context("parsing token response")
}

fn creds_from_response(v: &Value) -> Result<ClaudeCreds> {
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
    let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(86400);
    let refresh_in = v.get("refresh_token_expires_in").and_then(Value::as_u64);
    let scopes: Vec<String> = v
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let now = now_ms();
    Ok(ClaudeCreds {
        access_token,
        refresh_token,
        expires_at_ms: now + expires_in * 1000,
        refresh_token_expires_at_ms: refresh_in.map(|s| now + s * 1000),
        scopes,
        subscription_type: None,
        rate_limit_tier: None,
    })
}

/// Refresh (rotating refresh token — the new one must be persisted).
pub async fn refresh(http: &reqwest::Client, c: &ClaudeCreds) -> Result<ClaudeCreds> {
    let resp = exchange(
        http,
        json!({
            "grant_type": "refresh_token",
            "refresh_token": c.refresh_token,
            "client_id": CLIENT_ID,
            "scope": c.scopes.join(" "),
        }),
    )
    .await?;
    let mut fresh = creds_from_response(&resp)?;
    if fresh.scopes.is_empty() {
        fresh.scopes = c.scopes.clone();
    }
    fresh.subscription_type = c.subscription_type.clone();
    fresh.rate_limit_tier = c.rate_limit_tier.clone();
    Ok(fresh)
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}
