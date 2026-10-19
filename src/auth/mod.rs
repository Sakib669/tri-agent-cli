pub mod chatgpt;
pub mod claude;
pub mod gemini;
pub mod loopback;
pub mod pkce;

use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::config::Config;
use crate::providers::{create, ProviderId};
use crate::store::{ProviderCreds, Store};
use crate::tui;

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(format!("triagent/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Into::into)
}

/// Open the authorize URL in the user's browser, trying direct launches of
/// known browsers first (more reliable than xdg-open from odd environments).
pub fn open_browser(url: &str) {
    const BROWSERS: [&str; 7] = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "firefox",
        "xdg-open",
    ];
    for bin in BROWSERS {
        let mut cmd = std::process::Command::new(bin);
        cmd.arg(url);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        if cmd.spawn().is_ok() {
            return;
        }
    }
    if let Err(e) = open::that(url) {
        eprintln!("\n  could not open a browser ({e});\n  paste the URL printed above into your browser instead.\n");
    }
}

/// Browser OAuth login for one provider; stores the result (0600 file).
pub async fn login(provider: &ProviderId, store: &Store) -> Result<()> {
    let http = http_client()?;
    tui::info(&format!("Logging in to {}", provider.display_name()));
    tui::info("A browser window will open; complete the sign-in there.");
    let creds = match provider {
        ProviderId::Claude => ProviderCreds::Claude(claude::do_login(&http).await?),
        ProviderId::ChatGpt => ProviderCreds::ChatGpt(chatgpt::do_login(&http).await?),
        ProviderId::Gemini => ProviderCreds::Gemini(gemini::do_login(&http).await?),
    };
    store.put(creds)?;
    tui::ok(&format!(
        "{} login stored ({})",
        provider,
        Store::credentials_path().display()
    ));
    Ok(())
}

/// Remove local credentials; for ChatGPT also revoke the refresh token.
pub async fn logout(provider: &ProviderId, store: &Store) -> Result<()> {
    let http = http_client()?;
    if let Some(old) = store.remove(*provider)? {
        if let ProviderCreds::ChatGpt(c) = &old {
            let _ = chatgpt::revoke(&http, c).await; // best-effort remote revoke
        }
        tui::ok(&format!("logged out of {provider}"));
    } else {
        tui::warn(&format!("{provider}: no stored login"));
    }
    Ok(())
}

/// Offline account/plan summary + a best-effort usage probe.
pub async fn status(_config: &Config, store: &Store) -> Result<()> {
    let all = store.all()?;
    if all.is_empty() {
        tui::warn("no logins yet — run: triagent login <claude|chatgpt|gemini>");
        return Ok(());
    }
    let http = http_client()?;
    for (name, creds) in &all {
        let provider = creds.provider();
        let (plan, expires) = match creds {
            ProviderCreds::Claude(c) => (
                c.subscription_type
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                c.expires_at_ms,
            ),
            ProviderCreds::ChatGpt(c) => (
                plan_from_id_token(&c.id_token).unwrap_or_else(|| "unknown".into()),
                expiry_from_id_token(&c.id_token),
            ),
            ProviderCreds::Gemini(_) => ("consumer".to_string(), 0),
        };
        println!(
            "  {name:<8} ✓ logged in   plan/tier: {plan:<12} {}",
            if provider == ProviderId::Claude && expires > 0 {
                let left = expires.saturating_sub(now_ms()) / 1000;
                if left > 0 {
                    format!("access token expires in ~{left}s")
                } else {
                    "access token expired (will refresh)".into()
                }
            } else if provider == ProviderId::ChatGpt && expires > 0 {
                format!("id_token exp: {}", expires)
            } else {
                String::new()
            }
        );
    }
    // Best-effort usage probe (network may be down; never fatal).
    for creds in all.values() {
        let provider = creds.provider();
        if let Ok(prov) = create(creds.clone(), http.clone(), false) {
            if let Ok(u) = prov.usage().await {
                if !u.label.is_empty() {
                    println!("  {provider:<8} usage: {}", u.label);
                }
            }
        }
    }
    Ok(())
}

/// List subscription models for one provider.
pub async fn list_models(provider: &ProviderId, store: &Store) -> Result<()> {
    let creds = require_creds(provider, store)?;
    let fresh = ensure_fresh(provider, creds).await?;
    let http = http_client()?;
    let prov = create(fresh, http, false)?;
    let models = prov.list_models().await?;
    if models.is_empty() {
        bail!("no models returned by {provider}");
    }
    println!("{} models:", prov.id());
    for m in models {
        println!(
            "  {:<28} {}{}{}",
            m.id,
            m.display_name.as_deref().unwrap_or(""),
            m.context_window
                .map(|c| format!(" ({c} ctx)"))
                .unwrap_or_default(),
            m.quota
                .as_deref()
                .map(|q| format!("  [{q}]"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

pub fn require_creds(provider: &ProviderId, store: &Store) -> Result<ProviderCreds> {
    store
        .get(*provider)?
        .ok_or_else(|| anyhow!("no {provider} login — run: triagent login {provider}"))
}

/// Refresh access token when it is expired or within `window` of expiry.
pub async fn ensure_fresh(_provider: &ProviderId, creds: ProviderCreds) -> Result<ProviderCreds> {
    let http = http_client()?;
    let window = Duration::from_secs(300);
    let needs = match &creds {
        ProviderCreds::Claude(c) => {
            c.expires_at_ms.saturating_sub(now_ms()) < window.as_millis() as u64
        }
        ProviderCreds::ChatGpt(c) => c
            .last_refresh_expiry()
            .map(|e| e.saturating_sub(now_ms()) < window.as_millis() as u64)
            .unwrap_or(true),
        ProviderCreds::Gemini(c) => c
            .expiry_ms()
            .map(|e| e.saturating_sub(now_ms()) < window.as_millis() as u64)
            .unwrap_or(true),
    };
    if !needs {
        return Ok(creds);
    }
    let fresh = match &creds {
        ProviderCreds::Claude(c) => ProviderCreds::Claude(claude::refresh(&http, c).await?),
        ProviderCreds::ChatGpt(c) => ProviderCreds::ChatGpt(chatgpt::refresh(&http, c).await?),
        ProviderCreds::Gemini(c) => ProviderCreds::Gemini(gemini::refresh(&http, c).await?),
    };
    // Persist under a per-provider key; recompute provider id.
    let store = Store::open()?;
    store.put(fresh.clone())?;
    tui::dim(&format!("{} token refreshed", fresh.provider()));
    Ok(fresh)
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Decode the payload of a JWT without verifying the signature, and return a
/// claim string. Used only for display (plan tier, expiry) — never for trust.
pub fn jwt_claim(id_token: &str, claim: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let padded = {
        let m = payload.len() % 4;
        if m == 0 {
            payload.to_string()
        } else {
            format!("{payload}{}", "=".repeat(4 - m))
        }
    };
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let bytes = URL_SAFE_NO_PAD.decode(padded).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get(claim).and_then(|c| c.as_str()).map(str::to_string)
}

fn plan_from_id_token(id_token: &str) -> Option<String> {
    jwt_claim(id_token, "https://api.openai.com/auth.chatgpt_plan_type")
        .or_else(|| jwt_claim(id_token, "chatgpt_plan_type"))
}

fn expiry_from_id_token(id_token: &str) -> u64 {
    jwt_claim(id_token, "exp")
        .and_then(|e| e.parse().ok())
        .unwrap_or(0)
}
