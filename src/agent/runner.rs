//! The run orchestrator: shared agent core driving either the Claude
//! Code-style TUI or plain streamed output. Everything runs in one process.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::auth;
use crate::config::{self, Config};
use crate::providers::{
    create, ContentPart, Message, Provider, ProviderEvent, ProviderId, Role, TurnRequest,
};
use crate::skills::{self, Skill};
use crate::store::{sessions_dir, Store};
use crate::theme::Theme;
use crate::tui;
use crate::ui::{TuiUi, Ui};

use super::permissions::{Answer, AskBridge, Asker, Mode};
use super::tools::{tool_specs, ToolRuntime};

const MAX_TURNS: usize = 50;
const MAX_TOKENS: u32 = 16000;
const HELP: &str = "commands:\n  /model <id>  switch model\n  /status      usage/quota info\n  /cost        session token + cost totals\n  /permissions show permission mode\n  /compact     summarize the conversation to free context\n  /memory      edit the global memory file (TRIAGENT.md)\n  /doctor      local health checks\n  /sessions    list saved sessions\n  /resume <id> load a saved session\n  /login <provider>   browser login (claude|chatgpt|gemini)\n  /logout <provider>  remove login\n  /clear       reset conversation\n  /exit        quit (session is saved)\nShift+Tab cycles permission mode: default → accept-edits → plan → bypass.";

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: &Config,
    store: &Store,
    provider_id: &ProviderId,
    prompt: Option<&str>,
    model: Option<&str>,
    resume: Option<&str>,
    skip_permissions: bool,
    add_dirs: &[PathBuf],
    debug: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("current_dir")?;

    let creds = auth::require_creds(provider_id, store)?;
    let fresh = auth::ensure_fresh(provider_id, creds).await?;
    let http = auth::http_client()?;
    let provider = create(fresh, http, debug)?;

    let model: String = model
        .map(str::to_string)
        .or_else(|| config.default_model(*provider_id))
        .or_else(|| provider_id.builtin_default_model().map(str::to_string))
        .context("no model configured")?;

    let mode = if skip_permissions {
        Mode::Bypass
    } else {
        Mode::from_str(&config.permissions.default_mode)
    };
    let interactive = prompt.is_none();
    let task_depth = std::env::var("TRIAGENT_TASK_DEPTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let session_file = match resume {
        Some(id) if valid_session_id(id) => sessions_dir().join(format!("{id}.jsonl")),
        Some(id) => bail!("invalid --resume id {id:?} (use letters, digits, dashes)"),
        None => sessions_dir().join(new_session_id()),
    };
    let messages: Vec<Message> = if resume.is_some() && session_file.exists() {
        load_session(&session_file)?
    } else {
        Vec::new()
    };

    let memory = load_memory(&cwd);
    let skills = skills::discover(&config.skills.expanded());
    let mut system = build_system(&cwd, &memory);
    if !skills.is_empty() {
        system.push_str(&format!("\n{}\n", skills::prompt_index(&skills, 5000)));
    }

    if interactive && std::io::stdout().is_terminal() {
        run_tui(
            store,
            provider_id,
            provider,
            model,
            mode,
            messages,
            system,
            session_file,
            cwd,
            add_dirs.to_vec(),
            task_depth,
            debug,
            config.ui.theme.clone(),
            skills,
        )
        .await
    } else {
        run_plain(
            provider_id,
            provider,
            model,
            mode,
            messages,
            system,
            session_file,
            cwd,
            add_dirs.to_vec(),
            prompt,
            task_depth,
            debug,
            interactive,
            skills,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Shared core
// ---------------------------------------------------------------------------

struct CostTracker {
    input: u64,
    output: u64,
    usd: f64,
}

impl CostTracker {
    fn add(&mut self, provider: ProviderId, model: &str, i: Option<u64>, o: Option<u64>) {
        let i = i.unwrap_or(0);
        let o = o.unwrap_or(0);
        self.input += i;
        self.output += o;
        if provider == ProviderId::Claude {
            let (pi, po) = claude_price(model);
            self.usd += i as f64 / 1e6 * pi + o as f64 / 1e6 * po;
        }
    }
}

fn claude_price(model: &str) -> (f64, f64) {
    if model.contains("opus") {
        (15.0, 75.0)
    } else if model.contains("haiku") {
        (0.8, 4.0)
    } else {
        (3.0, 15.0) // sonnet and defaults
    }
}

struct Core {
    store: Store,
    provider_id: ProviderId,
    provider: Box<dyn Provider>,
    runtime: ToolRuntime,
    asker: Asker,
    messages: Vec<Message>,
    model: String,
    system: String,
    session_file: Option<PathBuf>,
    ui: std::sync::Arc<dyn Ui>,
    tui: Option<std::sync::Arc<TuiUi>>,
    cost: CostTracker,
    task_depth: usize,
    debug: bool,
    logged_out: bool,
    details: bool,
    show_thinking: bool,
    pending: Option<String>,
    system_base: String,
    output_style: bool,
    skills: Vec<Skill>,
    pending_quiet: bool,
}

impl Core {
    fn status_line(&self) -> (String, String) {
        let left = format!(
            "{} · {} · {}",
            self.provider_id,
            self.model,
            self.asker.mode().label()
        );
        let session = self
            .session_file
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let right = format!(
            "↑{} ↓{} · ${:.2} · {}",
            self.cost.input,
            self.cost.output,
            self.cost.usd,
            shorten(&session, 28)
        );
        (left, right)
    }

    fn update_status(&self) {
        let (l, r) = self.status_line();
        self.ui.set_status(l, r);
    }

    async fn reload_provider(&mut self) {
        self.logged_out = false;
        match self.store.get(self.provider_id) {
            Ok(Some(creds)) => {
                let fresh = auth::ensure_fresh(&self.provider_id, creds).await;
                match fresh.and_then(|c| create(c, auth::http_client()?, self.debug)) {
                    Ok(p) => {
                        self.provider = p;
                        self.ui.info("credentials reloaded");
                    }
                    Err(e) => self.ui.info(&format!("credential refresh failed: {e}")),
                }
            }
            Ok(None) => {
                self.logged_out = true;
                self.ui.info(&format!(
                    "no {} login — run /login {}",
                    self.provider_id, self.provider_id
                ));
            }
            Err(e) => self.ui.info(&format!("could not read credentials: {e}")),
        }
    }

    fn suspend(&self) {
        if let Some(t) = &self.tui {
            t.tui.suspend();
        }
    }

    fn resume(&self) {
        if let Some(t) = &self.tui {
            t.tui.resume();
        }
    }

    fn is_exiting(&self) -> bool {
        self.tui
            .as_ref()
            .map(|t| t.tui.is_exiting())
            .unwrap_or(false)
    }

    /// One full agent turn: stream → execute tool calls → repeat.
    async fn turn_chain(
        &mut self,
        cancel_rx: &mut Option<mpsc::UnboundedReceiver<()>>,
    ) -> Result<()> {
        if self.logged_out {
            self.ui.info("not logged in — run /login <provider> first");
            return Ok(());
        }
        let mut ctx_retries = 0usize;
        for _ in 0..MAX_TURNS {
            if let Some(rx) = cancel_rx {
                while rx.try_recv().is_ok() {} // drop stale cancel tokens
            }
            // Never let a turn die on a context-limit error.
            self.fit_context();
            let req = TurnRequest {
                system: self.system.clone(),
                messages: self.messages.clone(),
                tools: tool_specs(self.asker.mode(), self.task_depth),
                model: self.model.clone(),
                max_tokens: MAX_TOKENS,
            };
            self.ui.set_busy(true);
            let mut stream = self.provider.stream_turn(req);

            let mut text = String::new();
            let mut tool_calls: Vec<(String, String, String, Option<String>)> = Vec::new();
            let mut err: Option<String> = None;
            let mut interrupted = false;
            let mut in_tok: Option<u64> = None;
            let mut out_tok: Option<u64> = None;
            let started = Instant::now();

            loop {
                let item = match cancel_rx {
                    Some(rx) => tokio::select! {
                        _ = rx.recv() => { interrupted = true; None }
                        item = stream.next() => item,
                    },
                    None => tokio::select! {
                        _ = tokio::signal::ctrl_c() => { interrupted = true; None }
                        item = stream.next() => item,
                    },
                };
                let Some(item) = item else { break };
                match item {
                    Ok(ev) => match ev {
                        ProviderEvent::TextDelta(t) => {
                            text.push_str(&t);
                            self.ui.assistant_append(&t);
                        }
                        ProviderEvent::Reasoning(r) => {
                            if self.show_thinking {
                                self.ui.reasoning(&r);
                            }
                        }
                        ProviderEvent::ToolCall {
                            id,
                            name,
                            args_json,
                            thought_signature,
                        } => {
                            let display = name.strip_prefix("default_api:").unwrap_or(&name);
                            self.ui.tool_call(display, &shorten(&args_json, 120));
                            tool_calls.push((id, name, args_json, thought_signature));
                        }
                        ProviderEvent::Usage {
                            input_tokens,
                            output_tokens,
                        } => {
                            in_tok = in_tok.or(input_tokens);
                            out_tok = out_tok.or(output_tokens);
                        }
                        ProviderEvent::TurnEnd => {}
                        ProviderEvent::Error(e) => err = Some(e),
                    },
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
            drop(stream);
            self.ui.set_busy(false);
            self.cost
                .add(self.provider_id, &self.model, in_tok, out_tok);
            self.update_status();

            if interrupted {
                self.ui.info("[interrupted]");
                return Ok(());
            }
            if let Some(e) = err {
                // Provider said the request is too big: trim hard and retry once
                // or twice instead of failing the whole turn.
                if is_context_overflow(&e) && ctx_retries < 2 {
                    ctx_retries += 1;
                    let target = self.char_budget() / 2;
                    let dropped = self.evict_oldest(target);
                    self.ui.info(&format!(
                        "context limit hit — dropped {dropped} older messages and retrying"
                    ));
                    continue;
                }
                self.ui.tool_result(&e, true);
                return Ok(());
            }

            // Save the assistant message (text + tool calls).
            if !text.trim().is_empty() || !tool_calls.is_empty() {
                let mut parts = Vec::new();
                if !text.is_empty() {
                    parts.push(ContentPart::Text { text: text.clone() });
                }
                for (id, name, args_json, sig) in &tool_calls {
                    let input: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
                    parts.push(ContentPart::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input,
                        thought_signature: sig.clone(),
                    });
                }
                self.messages.push(Message {
                    role: Role::Assistant,
                    parts,
                });
                self.save_session();
            }

            if tool_calls.is_empty() {
                self.ui.info(&format!(
                    "⏺ {} in · {} out · {:.1}s",
                    in_tok.unwrap_or(0),
                    out_tok.unwrap_or(0),
                    started.elapsed().as_secs_f32()
                ));
                return Ok(());
            }

            // exit_plan_mode: present the plan, ask for approval.
            if tool_calls.len() == 1 && tool_calls[0].1 == "exit_plan_mode" {
                let args: Value = serde_json::from_str(&tool_calls[0].2).unwrap_or(Value::Null);
                let plan = args
                    .get("plan")
                    .and_then(Value::as_str)
                    .unwrap_or("(no plan text)")
                    .to_string();
                self.ui.info(&format!("Plan:\n{plan}"));
                let ans = self
                    .asker
                    .ask("Approve this plan and switch to execution mode?")
                    .await;
                let result = if ans == Answer::Yes {
                    self.asker.set_mode(Mode::AcceptEdits);
                    self.update_status();
                    self.ui.info("plan approved — executing");
                    "plan approved".to_string()
                } else {
                    "plan rejected — revise and present again via exit_plan_mode".to_string()
                };
                self.messages.push(Message {
                    role: Role::User,
                    parts: vec![ContentPart::ToolResult {
                        tool_use_id: tool_calls[0].0.clone(),
                        content: result,
                        is_error: false,
                    }],
                });
                self.save_session();
                continue;
            }

            // Execute tools, one result part per call.
            let mut results = Vec::new();
            for (id, name, args_json, _sig) in tool_calls {
                let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
                let real = name.strip_prefix("default_api:").unwrap_or(&name);
                let out = self.runtime.execute(real, &args, &mut self.asker).await;
                self.ui
                    .tool_result(&shorten(&out.content, 200), out.is_error);
                if real == "edit_file" && !out.is_error {
                    self.ui.diff(&diff_card(&args));
                }
                results.push(ContentPart::ToolResult {
                    tool_use_id: id,
                    content: out.content,
                    is_error: out.is_error,
                });
            }
            self.messages.push(Message {
                role: Role::User,
                parts: results,
            });
            self.save_session();
        }
        bail!(
            "reached the maximum of {MAX_TURNS} turns; conversation saved — resume it with /resume"
        )
    }

    fn undo(&mut self) {
        if let Some(pos) = self
            .messages
            .iter()
            .rposition(|m| m.role == Role::Assistant)
        {
            self.messages.truncate(pos);
        }
        if let Some(pos) = self.messages.iter().rposition(|m| m.role == Role::User) {
            self.messages.truncate(pos);
        }
        self.replay();
        self.ui.info("undid the last turn");
    }

    fn replay(&self) {
        self.ui.clear();
        for m in &self.messages {
            match m.role {
                Role::User => {
                    for p in &m.parts {
                        if let ContentPart::Text { text } = p {
                            self.ui.user(text);
                        }
                    }
                }
                Role::Assistant => {
                    for p in &m.parts {
                        match p {
                            ContentPart::Text { text } => self.ui.assistant_append(text),
                            ContentPart::ToolUse { name, .. } => self.ui.tool_call(name, ""),
                            ContentPart::ToolResult { content, .. } => {
                                self.ui.tool_result(&shorten(content, 60), false);
                            }
                        }
                    }
                }
            }
        }
    }

    fn new_session(&mut self) {
        self.messages.clear();
        self.session_file = Some(sessions_dir().join(new_session_id()));
        self.ui.clear();
        self.update_status();
        self.ui.info("started a new session");
    }

    fn compose_in_editor(&self) -> Option<String> {
        self.suspend();
        let tmp = std::env::temp_dir().join(format!("triagent-msg-{}.md", uuid::Uuid::new_v4()));
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{editor} {}",
                shell_quote(&tmp.display().to_string())
            ))
            .status();
        self.resume();
        std::fs::read_to_string(&tmp)
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    fn set_output_style(&mut self, verbose: bool) {
        self.output_style = verbose;
        let hint = if verbose {
            "\n\nStyle: be thorough — explain your reasoning step by step and cover edge cases.\n"
        } else {
            ""
        };
        self.system = format!("{}{}", self.system_base, hint);
    }

    async fn aux_review(&mut self, instruction: &str, data: String) {
        if data.trim().is_empty() {
            self.ui.info("nothing to review here");
            return;
        }
        self.ui.info(instruction);
        let mut msgs = self.messages.clone();
        msgs.push(Message {
            role: Role::User,
            parts: vec![ContentPart::Text {
                text: format!(
                    "{instruction}\n\n```\n{}\n```",
                    &data[..data.len().min(24000)]
                ),
            }],
        });
        let req = TurnRequest {
            system: "You are a senior engineer doing a requested review. Be concise and concrete."
                .into(),
            messages: msgs,
            tools: vec![],
            model: self.model.clone(),
            max_tokens: 2000,
        };
        let mut stream = self.provider.stream_turn(req);
        while let Some(item) = stream.next().await {
            match item {
                Ok(ProviderEvent::TextDelta(t)) => self.ui.assistant_append(&t),
                Ok(_) => {}
                Err(e) => {
                    self.ui.tool_result(&format!("review failed: {e}"), true);
                    return;
                }
            }
        }
    }

    fn git_diff(&self) -> String {
        std::process::Command::new("git")
            .args(["diff", "HEAD"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    fn git_log(&self) -> String {
        std::process::Command::new("git")
            .args(["log", "-n", "20", "--oneline"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    fn custom_commands(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut dirs = vec![config::config_dir().join("commands")];
        if let Ok(cwd) = std::env::current_dir() {
            dirs.push(cwd.join(".triagent").join("commands"));
        }
        for dir in dirs {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) != Some("md") {
                        continue;
                    }
                    let name = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if let Ok(raw) = std::fs::read_to_string(&p) {
                        let desc = raw
                            .lines()
                            .find(|l| l.starts_with("description:"))
                            .map(|l| l.trim_start_matches("description:").trim().to_string())
                            .unwrap_or_default();
                        out.push((name, desc));
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn run_custom(&self, name: &str, args: &str) -> Option<String> {
        let mut candidates = vec![config::config_dir()
            .join("commands")
            .join(format!("{name}.md"))];
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(
                cwd.join(".triagent")
                    .join("commands")
                    .join(format!("{name}.md")),
            );
        }
        for p in candidates {
            if let Ok(raw) = std::fs::read_to_string(&p) {
                // strip optional frontmatter
                let body = if let Some(rest) = raw.strip_prefix("---") {
                    rest.split_once("---")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or(raw)
                } else {
                    raw
                };
                let tmpl = body
                    .trim()
                    .replace("$ARGUMENTS", args)
                    .replace("$1", args.split_whitespace().next().unwrap_or(""))
                    .replace("$2", args.split_whitespace().nth(1).unwrap_or(""))
                    .replace("$3", args.split_whitespace().nth(2).unwrap_or(""));
                return Some(tmpl);
            }
        }
        None
    }

    async fn review(&mut self) {
        let diff = std::process::Command::new("git")
            .args(["diff", "HEAD"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        if diff.trim().is_empty() {
            self.ui.info("no git diff to review");
            return;
        }
        self.ui.info("reviewing git diff…");
        let mut msgs = self.messages.clone();
        msgs.push(Message {
            role: Role::User,
            parts: vec![ContentPart::Text {
                text: format!("Review this git diff for bugs, security issues and style. Be concise.\n\n```diff\n{}\n```", &diff[..diff.len().min(24000)]),
            }],
        });
        let req = TurnRequest {
            system: "You are a senior code reviewer.".into(),
            messages: msgs,
            tools: vec![],
            model: self.model.clone(),
            max_tokens: 2000,
        };
        let mut stream = self.provider.stream_turn(req);
        while let Some(item) = stream.next().await {
            match item {
                Ok(ProviderEvent::TextDelta(t)) => self.ui.assistant_append(&t),
                Ok(_) => {}
                Err(e) => {
                    self.ui.tool_result(&format!("review failed: {e}"), true);
                    return;
                }
            }
        }
    }

    async fn switch_provider(&mut self, provider: ProviderId) {
        if provider == self.provider_id {
            self.ui.info(&format!("already using {provider}"));
            return;
        }
        self.provider_id = provider;
        let default_model = provider
            .builtin_default_model()
            .map(str::to_string)
            .unwrap_or_else(|| "auto".into());
        self.model = default_model.clone();
        self.runtime.set_provider_model(provider, default_model);
        self.reload_provider().await;
        self.update_status();
    }

    async fn show_model_picker(&mut self) {
        match self.provider.list_models().await {
            Ok(models) if !models.is_empty() => {
                let items: Vec<(String, String)> = models
                    .iter()
                    .map(|m| (m.id.clone(), m.quota.clone().unwrap_or_default()))
                    .collect();
                let has_tui = self.tui.is_some();
                self.ui.show_picker("models", "/model ", items);
                if has_tui {
                    self.ui.info("pick a model (↑/↓, Enter, Esc)");
                } else {
                    self.ui.info("switch with /model <id>");
                }
            }
            _ => self.ui.info("could not list models for this provider"),
        }
    }

    /// Character budget for the conversation: 85% of the model window,
    /// leaving room for tools and the reply.
    fn char_budget(&self) -> usize {
        context_window(&self.model) * 4 * 85 / 100
    }

    /// Drop the oldest messages until the transcript fits `target` chars.
    /// Keeps the newest messages so work in progress is never lost.
    fn evict_oldest(&mut self, target: usize) -> usize {
        let mut dropped = 0;
        while chars_of(&self.messages) > target && self.messages.len() > 4 {
            self.messages.remove(0);
            dropped += 1;
        }
        // Still too big with only a few messages: the overflow is one huge tool
        // output (a big file read, a long build log). Shrink it in place.
        let mut shrunk = 0;
        while chars_of(&self.messages) > target && shrunk < 8 {
            let Some((mi, largest, _)) = self
                .messages
                .iter()
                .enumerate()
                .flat_map(|(mi, m)| m.parts.iter().enumerate().map(move |(pi, p)| (mi, pi, p)))
                .filter_map(|(mi, pi, p)| match p {
                    ContentPart::ToolResult { content, .. } if content.chars().count() > 2_000 => {
                        Some((mi, pi, content.chars().count()))
                    }
                    _ => None,
                })
                .max_by_key(|(_, _, len)| *len)
            else {
                break;
            };
            if let Some(ContentPart::ToolResult { content, .. }) =
                self.messages[mi].parts.get_mut(largest)
            {
                *content = shorten(content, 4_000).to_string();
            }
            shrunk += 1;
        }
        if dropped > 0 {
            self.messages.insert(
                0,
                Message {
                    role: Role::User,
                    parts: vec![ContentPart::Text {
                        text: format!(
                            "[{dropped} earlier messages were removed to fit the model's                              context window; continue from the most recent message.]"
                        ),
                    }],
                },
            );
        }
        dropped
    }

    /// Trim silently before a provider call so the request always fits.
    fn fit_context(&mut self) {
        let budget = self.char_budget();
        let total = chars_of(&self.messages) + self.system.chars().count() + 24_000;
        if total <= budget {
            return;
        }
        let target = (budget * 3 / 4).saturating_sub(self.system.chars().count());
        let dropped = self.evict_oldest(target);
        if dropped > 0 {
            self.ui.info(&format!(
                "context window nearly full — dropped {dropped} older messages"
            ));
        }
    }

    /// One small summarization request (never the whole transcript).
    async fn summarize(&self, prompt: &str) -> Result<String> {
        let req = TurnRequest {
            system: "You summarize coding sessions so work can continue in a fresh window. \
                     Reply with the summary only."
                .into(),
            messages: vec![Message {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: prompt.to_string(),
                }],
            }],
            tools: vec![],
            model: self.model.clone(),
            max_tokens: 1500,
        };
        let mut stream = self.provider.stream_turn(req);
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(ProviderEvent::TextDelta(t)) => text.push_str(&t),
                Ok(_) => {}
                Err(e) => bail!("{e}"),
            }
        }
        Ok(text)
    }

    /// Compact by summarizing in chunks. Every request stays small, so this
    /// still works when the conversation is already over the limit.
    async fn compact(&mut self) {
        if self.messages.is_empty() {
            self.ui.info("nothing to compact");
            return;
        }
        let before = chars_of(&self.messages);
        self.ui.info("compacting conversation…");

        // Sum of the original user turns, so intent survives even if summarization fails.
        // Big enough to be efficient, small enough that each summarization
        // request fits comfortably in the window.
        let chunk_budget = (self.char_budget() / 12).clamp(12_000, 60_000);
        let chunks = chunk_messages(&self.messages, chunk_budget);
        let mut summary = String::new();
        let mut summarized = 0usize;

        let total_chunks = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            if total_chunks > 1 {
                self.ui
                    .info(&format!("summarizing part {}/{}…", i + 1, total_chunks));
            }
            let prompt = if summary.is_empty() {
                format!(
                    "Summarize this part of a coding session concisely for continuation: what \
                     the user asked, what was done, which files changed, decisions made, and \
                     anything still open.\n\n{chunk}"
                )
            } else {
                format!(
                    "Running summary so far:\n{summary}\n\nMerge the next part into it. \
                     Reply with the updated summary only.\n\n{chunk}"
                )
            };
            match self.summarize(&prompt).await {
                Ok(s) if !s.trim().is_empty() => {
                    summary = s.trim().to_string();
                    summarized += 1;
                }
                Ok(_) => {}
                Err(e) => self.ui.info(&format!("skipping one chunk: {e}")),
            }
        }

        // Keep the last exchange verbatim for immediate continuity.
        let tail: Vec<Message> = self.messages.iter().rev().take(2).rev().cloned().collect();

        let note = if summary.is_empty() {
            self.ui
                .info("summarizer unavailable — keeping the recent messages verbatim");
            String::from(
                "[Conversation compacted. The earlier transcript exceeded the context window \
                 and was dropped; continue from the messages that follow.]",
            )
        } else {
            format!(
                "[Conversation compacted from {before} chars ({} chunk(s) summarized)]\n{summary}",
                summarized
            )
        };

        let mut msgs = vec![Message {
            role: Role::User,
            parts: vec![ContentPart::Text { text: note }],
        }];
        msgs.extend(tail);
        self.messages = msgs;
        self.save_session();
        self.ui.info(&format!(
            "conversation compacted: {} chars → {} chars",
            before,
            chars_of(&self.messages)
        ));
    }

    /// Rewrite the session file from the in-memory transcript. Always a full
    /// rewrite: after a compaction or context trim the file must not keep
    /// messages the conversation no longer has, or resuming re-breaks it.
    fn save_session(&self) {
        let Some(path) = &self.session_file else {
            return;
        };
        let mut body = String::new();
        for m in &self.messages {
            let Ok(json) = serde_json::to_string(m) else {
                continue;
            };
            body.push_str(&json);
            body.push('\n');
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            let _ = f.write_all(body.as_bytes());
        }
    }
}

fn diff_card(args: &Value) -> String {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let old = args.get("old_string").and_then(Value::as_str).unwrap_or("");
    let new = args.get("new_string").and_then(Value::as_str).unwrap_or("");
    let mut s = format!("--- {path}\n");
    for l in old.lines() {
        s.push('-');
        s.push_str(l);
        s.push('\n');
    }
    for l in new.lines() {
        s.push('+');
        s.push_str(l);
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------------------
// Plain (non-TUI) UI
// ---------------------------------------------------------------------------

struct PlainUi;

impl Ui for PlainUi {
    fn user(&self, text: &str) {
        println!("{}", tui::bold(&format!("❯ {text}")));
    }
    fn assistant_append(&self, text: &str) {
        tui::streamed(text);
    }
    fn reasoning(&self, text: &str) {
        tui::tool_line(&format!("  ⋮ {text}"));
    }
    fn tool_call(&self, name: &str, args: &str) {
        tui::tool_line(&format!("  ⚙ {name} {args}"));
    }
    fn tool_result(&self, content: &str, is_error: bool) {
        if is_error {
            tui::error(&format!("  ⤷ {content}"));
        } else {
            tui::tool_line(&format!("  ⤷ {content}"));
        }
    }
    fn info(&self, text: &str) {
        println!("{}", tui::dim(text));
    }
    fn diff(&self, text: &str) {
        println!("{text}");
    }
    fn set_busy(&self, _busy: bool) {}
    fn set_status(&self, left: String, right: String) {
        tui::dim(&format!("{left} · {right}"));
    }
    fn set_theme(&self, _name: String) {}
    fn show_picker(&self, _title: &str, _prefix: &str, items: Vec<(String, String)>) {
        for (id, desc) in items {
            println!("  {:<26} {}", id, desc);
        }
    }
    fn clear(&self) {}
}

// ---------------------------------------------------------------------------
// Plain mode (print / piped REPL)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_plain(
    provider_id: &ProviderId,
    provider: Box<dyn Provider>,
    model: String,
    mode: Mode,
    messages: Vec<Message>,
    system: String,
    session_file: PathBuf,
    cwd: PathBuf,
    add_dirs: Vec<PathBuf>,
    prompt: Option<&str>,
    task_depth: usize,
    debug: bool,
    interactive: bool,
    skills: Vec<Skill>,
) -> Result<()> {
    let ui: std::sync::Arc<dyn Ui> = std::sync::Arc::new(PlainUi);
    let runtime = ToolRuntime::new(cwd, add_dirs, *provider_id, model.clone());
    let asker = Asker::new(interactive, mode, None);
    let mut core = Core {
        store: Store::open()?,
        provider_id: *provider_id,
        provider,
        runtime,
        asker,
        messages,
        model,
        system: system.clone(),
        session_file: Some(session_file),
        ui,
        tui: None,
        cost: CostTracker {
            input: 0,
            output: 0,
            usd: 0.0,
        },
        task_depth,
        debug,
        logged_out: false,
        details: false,
        show_thinking: true,
        pending: None,
        system_base: system.clone(),
        output_style: false,
        skills,
        pending_quiet: false,
    };
    core.update_status();

    if let Some(p) = prompt {
        core.ui.user(p);
        core.messages.push(Message {
            role: Role::User,
            parts: vec![ContentPart::Text {
                text: p.to_string(),
            }],
        });
        core.save_session();
        core.turn_chain(&mut None).await?;
        println!();
        return Ok(());
    }

    loop {
        let line = read_user_input().await?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "/exit" || line == "/quit" || line == "/q" {
            break;
        }
        if let Some(cmd) = line.strip_prefix('!') {
            let out = core.runtime.shell_unprompted(cmd).await;
            core.ui.user(&format!("!{cmd}"));
            core.ui
                .tool_result(&shorten(&out.content, 200), out.is_error);
            core.messages.push(Message {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: format!("!{cmd}"),
                }],
            });
            core.messages.push(Message {
                role: Role::User,
                parts: vec![ContentPart::ToolResult {
                    tool_use_id: uuid::Uuid::new_v4().to_string(),
                    content: out.content,
                    is_error: out.is_error,
                }],
            });
            core.save_session();
            continue;
        }
        if handle_command(&mut core, &line).await? {
            continue;
        }
        let text = core.pending.take().unwrap_or(line);
        let quiet = core.pending_quiet;
        core.pending_quiet = false;
        if !quiet {
            core.ui.user(&text);
        }
        core.messages.push(Message {
            role: Role::User,
            parts: vec![ContentPart::Text { text }],
        });
        core.save_session();
        if let Err(e) = core.turn_chain(&mut None).await {
            core.ui.tool_result(&format!("{e:#}"), true);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TUI mode
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_tui(
    store: &Store,
    provider_id: &ProviderId,
    provider: Box<dyn Provider>,
    model: String,
    mode: Mode,
    messages: Vec<Message>,
    system: String,
    session_file: PathBuf,
    cwd: PathBuf,
    add_dirs: Vec<PathBuf>,
    task_depth: usize,
    debug: bool,
    theme: String,
    skills: Vec<Skill>,
) -> Result<()> {
    let mut tui_ui = TuiUi::new()?;
    // Take the receivers before sharing the UI (they are single-consumer).
    let submit_rx = tui_ui.tui.submit_rx.take().expect("submit rx");
    let mode_rx = tui_ui.tui.mode_rx.take().expect("mode rx");
    let cancel_rx = tui_ui.tui.cancel_rx.take().expect("cancel rx");
    let ask_tx = tui_ui.tui.ask_channel();
    let handle = std::sync::Arc::new(tui_ui);
    let ui: std::sync::Arc<dyn Ui> = handle.clone();
    let _ = store;

    let bridge = AskBridge(ask_tx);
    let runtime = ToolRuntime::new(cwd, add_dirs, *provider_id, model.clone());
    let asker = Asker::new(true, mode, Some(bridge));
    let mut core = Core {
        store: Store::open()?,
        provider_id: *provider_id,
        provider,
        runtime,
        asker,
        messages,
        model,
        system: system.clone(),
        session_file: Some(session_file),
        ui: ui.clone(),
        tui: Some(handle.clone()),
        cost: CostTracker {
            input: 0,
            output: 0,
            usd: 0.0,
        },
        task_depth,
        debug,
        logged_out: false,
        details: false,
        show_thinking: true,
        pending: None,
        system_base: system.clone(),
        output_style: false,
        skills,
        pending_quiet: false,
    };
    core.ui.set_theme(theme);
    core.ui.info("triagent — Claude Code-style TUI");
    core.update_status();

    let session_short = core
        .session_file
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    core.ui.info(&format!(
        "session {session_short} · Ctrl+C interrupt · Ctrl+D exit"
    ));

    let mut submit_rx = submit_rx;
    let mut mode_rx = mode_rx;
    let mut cancel_opt = Some(cancel_rx);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = mode_rx.recv() => {
                let next = core.asker.mode().next();
                core.asker.set_mode(next);
                core.update_status();
                core.ui.info(&format!("mode → {}", next.label()));
            }
            line = submit_rx.recv() => {
                let Some(line) = line else { break };
                let line = line.trim().to_string();
                if line.is_empty() { continue; }
                if line == "/exit" || line == "/quit" || line == "/q" {
                    break;
                }
                if let Some(cmd) = line.strip_prefix('!') {
                    let out = core
                        .runtime
                        .shell_unprompted(cmd)
                        .await;
                    core.ui.user(&format!("!{cmd}"));
                    core.ui.tool_result(&shorten(&out.content, 200), out.is_error);
                    core.messages.push(Message {
                        role: Role::User,
                        parts: vec![ContentPart::Text {
                            text: format!("!{cmd}"),
                        }],
                    });
                    core.messages.push(Message {
                        role: Role::User,
                        parts: vec![ContentPart::ToolResult {
                            tool_use_id: uuid::Uuid::new_v4().to_string(),
                            content: out.content,
                            is_error: out.is_error,
                        }],
                    });
                    core.save_session();
                    continue;
                }
                if handle_command(&mut core, &line).await? {
                    continue;
                }
                let text = core.pending.take().unwrap_or(line);
                let quiet = core.pending_quiet;
                core.pending_quiet = false;
                if !quiet {
                    core.ui.user(&text);
                }
                core.messages.push(Message {
                    role: Role::User,
                    parts: vec![ContentPart::Text { text }],
                });
                core.save_session();
                if let Err(e) = core.turn_chain(&mut cancel_opt).await {
                    core.ui.tool_result(&format!("{e:#}"), true);
                }
            }
            _ = tick.tick() => {
                if core.is_exiting() {
                    break;
                }
            }
        }
    }
    handle.tui.request_exit();
    Ok(())
}

// ---------------------------------------------------------------------------
// Slash commands (shared)
// ---------------------------------------------------------------------------

/// Returns Ok(true) when the line was handled as a command.
async fn handle_command(core: &mut Core, line: &str) -> Result<bool> {
    match line {
        "/help" => {
            core.ui.info(HELP);
            Ok(true)
        }
        "/clear" => {
            core.messages.clear();
            core.ui.clear();
            core.ui.info("conversation cleared");
            Ok(true)
        }
        "/status" => match core.provider.usage().await {
            Ok(u) if !u.label.is_empty() => {
                core.ui.info(&u.label);
                Ok(true)
            }
            Ok(_) => {
                core.ui.info("no usage info available");
                Ok(true)
            }
            Err(e) => {
                core.ui.info(&format!("usage probe failed: {e}"));
                Ok(true)
            }
        },
        "/cost" => {
            core.ui.info(&format!(
                "session: ↑{} in · ↓{} out · ${:.2}",
                core.cost.input, core.cost.output, core.cost.usd
            ));
            Ok(true)
        }
        "/permissions" => {
            core.ui
                .info(&format!("permission mode: {}", core.asker.mode().label()));
            Ok(true)
        }
        "/model" => {
            core.show_model_picker().await;
            Ok(true)
        }
        "/doctor" => {
            for l in doctor(&core.store) {
                core.ui.info(&l);
            }
            Ok(true)
        }
        "/sessions" => {
            for l in list_sessions() {
                core.ui.info(&l);
            }
            Ok(true)
        }
        "/memory" => {
            let path = memory_path();
            core.ui.info(&format!("editing {}", path.display()));
            core.suspend();
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "{editor} {}",
                    shell_quote(&path.display().to_string())
                ))
                .status();
            core.resume();
            Ok(true)
        }
        cmd if cmd.starts_with("/model ") => {
            let m = cmd.trim_start_matches("/model ").trim().to_string();
            if m.is_empty() {
                core.show_model_picker().await;
            } else {
                core.model = m;
                core.update_status();
                core.ui.info("model updated");
            }
            Ok(true)
        }
        cmd if cmd.starts_with("/resume ") => {
            let id = cmd.trim_start_matches("/resume ").trim();
            if !valid_session_id(id) {
                core.ui.info("invalid session id");
                return Ok(true);
            }
            let path = sessions_dir().join(format!("{id}.jsonl"));
            match load_session(&path) {
                Ok(msgs) => {
                    core.messages = msgs;
                    core.ui.clear();
                    core.ui.info(&format!("resumed session {id}"));
                }
                Err(e) => core.ui.info(&format!("resume failed: {e:#}")),
            }
            Ok(true)
        }
        cmd if cmd.starts_with("/login ") => {
            let provider: ProviderId = cmd.trim_start_matches("/login ").trim().parse()?;
            core.suspend();
            let result = auth::login(&provider, &core.store).await;
            core.resume();
            match result {
                Ok(()) => {
                    core.ui.info(&format!("{provider} login stored"));
                    if provider == core.provider_id {
                        core.reload_provider().await;
                    }
                }
                Err(e) => core.ui.info(&format!("login failed: {e:#}")),
            }
            Ok(true)
        }
        cmd if cmd.starts_with("/logout ") => {
            let provider: ProviderId = cmd.trim_start_matches("/logout ").trim().parse()?;
            core.suspend();
            let result = auth::logout(&provider, &core.store).await;
            core.resume();
            match result {
                Ok(()) => {
                    core.ui.info(&format!("{provider} logged out"));
                    if provider == core.provider_id {
                        core.reload_provider().await;
                    }
                }
                Err(e) => core.ui.info(&format!("logout failed: {e:#}")),
            }
            Ok(true)
        }
        "/compact" => {
            core.compact().await;
            Ok(true)
        }
        "/pwd" => {
            let cwd = std::env::current_dir().unwrap_or_default();
            core.ui
                .info(&format!("working directory: {}", cwd.display()));
            Ok(true)
        }
        "/config" => {
            let path = config::config_path();
            core.ui.info(&format!("config file: {}", path.display()));
            if let Ok(raw) = std::fs::read_to_string(&path) {
                for l in raw.lines().take(20) {
                    core.ui.info(l);
                }
            }
            Ok(true)
        }
        "/init" => {
            let cwd = std::env::current_dir().unwrap_or_default();
            let path = cwd.join("TRIAGENT.md");
            if path.exists() {
                core.ui.info(&format!("{} already exists", path.display()));
                Ok(true)
            } else {
                let template = "# TRIAGENT.md\n\nProject instructions for triagent. The agent reads this file automatically.\n\n## Build / test commands\n- \n\n## Code style\n- \n";
                match std::fs::write(&path, template) {
                    Ok(()) => {
                        core.ui.info(&format!("created {}", path.display()));
                    }
                    Err(e) => core.ui.info(&format!("could not create file: {e}")),
                }
                Ok(true)
            }
        }
        "/export" => {
            let session = core
                .session_file
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".into());
            let path = std::env::current_dir()
                .unwrap_or_default()
                .join(format!("triagent-export-{session}.md"));
            let mut md = String::new();
            for m in &core.messages {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                md.push_str(&format!("## {role}\n\n"));
                for p in &m.parts {
                    match p {
                        ContentPart::Text { text } => md.push_str(&format!("{text}\n\n")),
                        ContentPart::ToolUse { name, input, .. } => {
                            md.push_str(&format!("*tool: {name}*\n\n```json\n{input}\n```\n\n"));
                        }
                        ContentPart::ToolResult { content, .. } => {
                            md.push_str(&format!(
                                "*result:*\n\n```\n{}\n```\n\n",
                                shorten(content, 2000)
                            ));
                        }
                    }
                }
            }
            match std::fs::write(&path, md) {
                Ok(()) => core.ui.info(&format!("exported to {}", path.display())),
                Err(e) => core.ui.info(&format!("export failed: {e}")),
            }
            Ok(true)
        }
        "/summarize" => {
            core.compact().await;
            Ok(true)
        }
        "/usage" => {
            core.ui.info(&format!(
                "session: ↑{} in · ↓{} out · ${:.2}",
                core.cost.input, core.cost.output, core.cost.usd
            ));
            Ok(true)
        }
        "/undo" | "/rewind" => {
            core.undo();
            Ok(true)
        }
        "/redo" => {
            core.ui.info("nothing to redo (undo does not stack yet)");
            Ok(true)
        }
        "/new" => {
            core.new_session();
            Ok(true)
        }
        "/models" => {
            core.show_model_picker().await;
            Ok(true)
        }
        "/details" => {
            core.details = !core.details;
            core.ui.info(&format!(
                "tool details: {}",
                if core.details { "on" } else { "off" }
            ));
            Ok(true)
        }
        "/thinking" => {
            core.show_thinking = !core.show_thinking;
            core.ui.info(&format!(
                "reasoning display: {}",
                if core.show_thinking { "on" } else { "off" }
            ));
            Ok(true)
        }
        "/editor" => {
            match core.compose_in_editor() {
                Some(text) => {
                    core.pending = Some(text);
                    Ok(false) // caller submits pending as a user message
                }
                None => {
                    core.ui.info("editor cancelled or empty");
                    Ok(true)
                }
            }
        }
        "/themes" => {
            for name in Theme::names() {
                core.ui.info(name);
            }
            Ok(true)
        }
        cmd if cmd.starts_with("/theme ") => {
            let name = cmd.trim_start_matches("/theme ").trim().to_string();
            let known = Theme::names().into_iter().any(|n| n == name);
            if !known {
                core.ui
                    .info(&format!("unknown theme {name:?} — run /themes"));
                return Ok(true);
            }
            core.ui.set_theme(name.clone());
            persist_theme(&name);
            core.ui.info(&format!("theme → {name}"));
            Ok(true)
        }
        cmd if cmd.starts_with("/provider ") => {
            let provider: ProviderId = cmd.trim_start_matches("/provider ").trim().parse()?;
            core.switch_provider(provider).await;
            Ok(true)
        }
        cmd if cmd.starts_with("/connect ") => {
            let provider: ProviderId = cmd.trim_start_matches("/connect ").trim().parse()?;
            core.suspend();
            let result = auth::login(&provider, &core.store).await;
            core.resume();
            match result {
                Ok(()) => {
                    core.ui.info(&format!("{provider} login stored"));
                    if provider == core.provider_id {
                        core.reload_provider().await;
                    }
                }
                Err(e) => core.ui.info(&format!("login failed: {e:#}")),
            }
            Ok(true)
        }
        "/review" => {
            core.review().await;
            Ok(true)
        }
        cmd if cmd.starts_with("/add-dir ") => {
            let dir = std::path::PathBuf::from(cmd.trim_start_matches("/add-dir ").trim());
            core.runtime.add_dir(dir.clone());
            core.ui.info(&format!("added directory: {}", dir.display()));
            Ok(true)
        }
        "/todos" => {
            core.ui
                .info("todo items are tracked by the todo tool during a turn");
            Ok(true)
        }
        "/keybinds" => {
            core.ui.info("keybinds: / menu · ? help · Tab/Shift+Tab mode · Ctrl+C interrupt (×2 exit) · Ctrl+D exit · Ctrl+L redraw · PgUp/PgDn scroll · ↑/↓ history · !cmd run shell · @file reference");
            Ok(true)
        }
        "/upgrade" => {
            core.ui
                .info(&format!("triagent {}", env!("CARGO_PKG_VERSION")));
            Ok(true)
        }
        "/agents" => {
            core.ui.info(
                "the `task` tool spawns subagents on the same subscription (depth-limited to 1)",
            );
            Ok(true)
        }
        "/prompts" => {
            let cmds = core.custom_commands();
            if cmds.is_empty() {
                core.ui.info("no custom commands — add markdown files to ~/.config/triagent/commands/ or ./.triagent/commands/");
            } else {
                for (name, desc) in cmds {
                    core.ui.info(&format!("/{name:<16} {desc}"));
                }
            }
            Ok(true)
        }
        "/output-style" => {
            core.set_output_style(!core.output_style);
            core.ui.info(&format!(
                "output style: {}",
                if core.output_style {
                    "explain everything"
                } else {
                    "default"
                }
            ));
            Ok(true)
        }
        "/security-review" => {
            let diff = core.git_diff();
            core.aux_review(
                "Review this git diff for security vulnerabilities and unsafe patterns.",
                diff,
            )
            .await;
            Ok(true)
        }
        "/pr-comments" => {
            let diff = core.git_diff();
            core.aux_review(
                "Review this git diff and suggest concrete PR comments.",
                diff,
            )
            .await;
            Ok(true)
        }
        "/release-notes" => {
            let log = core.git_log();
            core.aux_review("Draft user-facing release notes from this git log.", log)
                .await;
            Ok(true)
        }
        "/bug" => {
            let remote = std::process::Command::new("git")
                .args(["remote", "get-url", "origin"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "(no git remote)".into());
            core.ui.info(&format!(
                "triagent {} · {}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            ));
            core.ui.info(&format!("repo: {remote}"));
            core.ui.info(
                "include: what you ran, expected vs actual, and the session id in the top bar.",
            );
            Ok(true)
        }
        "/skills" => {
            if core.skills.is_empty() {
                core.ui
                    .info("no skills found — add SKILL.md folders to ~/.config/triagent/skills/");
            } else {
                for s in &core.skills {
                    core.ui
                        .info(&format!("/{:<28} {}", s.name, s.short_desc(70)));
                }
            }
            Ok(true)
        }
        cmd if cmd.starts_with("/skill ") => {
            let name = cmd.trim_start_matches("/skill ").trim();
            match core.skills.iter().find(|s| s.name == name) {
                Some(skill) => match skill.body() {
                    Some(body) => {
                        core.ui.info(&format!("loaded skill: {}", skill.name));
                        core.pending_quiet = true;
                        core.pending = Some(format!(
                            "Use the following skill for this request.\n\n# Skill: {}\n\n{}\n",
                            skill.name, body
                        ));
                        Ok(false)
                    }
                    None => {
                        core.ui.info("could not read that skill");
                        Ok(true)
                    }
                },
                None => {
                    core.ui
                        .info(&format!("unknown skill {name:?} — run /skills"));
                    Ok(true)
                }
            }
        }
        cmd if cmd.starts_with('/') => {
            // custom command files: /name [args]
            let (name, args) = match cmd.split_once(' ') {
                Some((n, a)) => (n.trim_start_matches('/'), a.trim()),
                None => (cmd.trim_start_matches('/'), ""),
            };
            if let Some(tmpl) = core.run_custom(name, args) {
                core.pending = Some(tmpl);
                core.pending_quiet = true;
                return Ok(false); // caller submits it as a user message
            }
            if let Some(skill) = core.skills.iter().find(|s| s.name == name) {
                if let Some(body) = skill.body() {
                    let mut text = format!(
                        "Use the following skill for this request.\n\n# Skill: {}\n\n{}\n",
                        skill.name, body
                    );
                    if !args.is_empty() {
                        text.push_str(&format!("\nUser request: {args}\n"));
                    }
                    core.pending = Some(text);
                    core.pending_quiet = true;
                    core.ui.info(&format!("loaded skill: {}", skill.name));
                    return Ok(false);
                }
            }
            core.ui.info(&format!("unknown command {cmd} — try /help"));
            Ok(true)
        }
        _ => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn doctor(store: &Store) -> Vec<String> {
    let mut out = vec![
        format!("triagent {}", env!("CARGO_PKG_VERSION")),
        format!("config: {}", config::config_path().display()),
        format!("credentials: {}", Store::credentials_path().display()),
    ];
    match store.all() {
        Ok(all) => {
            if all.is_empty() {
                out.push("logins: none".into());
            } else {
                for (name, _) in all {
                    out.push(format!("login: {name} ✓"));
                }
            }
        }
        Err(e) => out.push(format!("credentials unreadable: {e}")),
    }
    if let Ok(md) = std::fs::metadata(Store::credentials_path()) {
        use std::os::unix::fs::PermissionsExt;
        out.push(format!(
            "credentials mode: {:o}",
            md.permissions().mode() & 0o777
        ));
    }
    out.push(format!("saved sessions: {}", list_sessions().len()));
    out
}

fn list_sessions() -> Vec<String> {
    let mut entries: Vec<(std::time::SystemTime, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".jsonl") {
                let mtime = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                entries.push((mtime, name));
            }
        }
    }
    entries.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    entries
        .into_iter()
        .take(20)
        .map(|(_, name)| name.trim_end_matches(".jsonl").to_string())
        .collect()
}

fn memory_path() -> PathBuf {
    config::config_dir().join("TRIAGENT.md")
}

fn load_memory(cwd: &Path) -> String {
    let mut parts = Vec::new();
    let global = memory_path();
    if let Ok(s) = std::fs::read_to_string(&global) {
        parts.push(format!("Global memory ({}):\n{s}", global.display()));
    }
    let proj = cwd.join("TRIAGENT.md");
    if let Ok(s) = std::fs::read_to_string(&proj) {
        parts.push(format!("Project memory ({}):\n{s}", proj.display()));
    }
    let mem = parts.join("\n\n");
    shorten(&mem, 4000)
}

/// Context window (tokens) for a model id. `TRIAGENT_CONTEXT_TOKENS` overrides
/// it for models the built-in table does not know.
fn context_window(model: &str) -> usize {
    if let Ok(v) = std::env::var("TRIAGENT_CONTEXT_TOKENS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n >= 1000 {
                return n;
            }
        }
    }
    let m = model.to_ascii_lowercase();
    if m.contains("gemini") {
        1_000_000
    } else if m.contains("gpt-5") || m.contains("codex") {
        400_000
    } else if m.contains("claude") {
        200_000
    } else if m.contains("gpt-4") {
        128_000
    } else {
        200_000
    }
}

fn msg_chars(m: &Message) -> usize {
    m.parts
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => text.chars().count() + 8,
            ContentPart::ToolUse { name, input, .. } => {
                name.chars().count() + input.to_string().chars().count() + 16
            }
            ContentPart::ToolResult { content, .. } => content.chars().count() + 16,
        })
        .sum()
}

fn chars_of(messages: &[Message]) -> usize {
    messages.iter().map(msg_chars).sum()
}

/// Flatten messages to plain text for summarization.
fn msg_to_text(m: &Message) -> String {
    let who = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut out = String::new();
    for p in &m.parts {
        match p {
            ContentPart::Text { text } => {
                out.push_str(who);
                out.push_str(": ");
                out.push_str(text);
                out.push('\n');
            }
            ContentPart::ToolUse { name, input, .. } => {
                out.push_str(&format!("[tool {name}] {input}\n"));
            }
            ContentPart::ToolResult { content, .. } => {
                out.push_str(&format!("[result] {}\n", shorten(content, 2000)));
            }
        }
    }
    out
}

/// Split the transcript into chunks of at most `budget` chars.
fn chunk_messages(messages: &[Message], budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for m in messages {
        let text = msg_to_text(m);
        if !cur.is_empty() && cur.chars().count() + text.chars().count() > budget {
            chunks.push(std::mem::take(&mut cur));
        }
        if text.chars().count() > budget {
            // A single huge message: keep head and tail so both framing survive.
            let head: String = text.chars().take(budget / 2).collect();
            let tail: String = {
                let total = text.chars().count();
                text.chars()
                    .skip(total.saturating_sub(budget / 2))
                    .collect()
            };
            cur.push_str(&head);
            cur.push_str("\n…[truncated]…\n");
            cur.push_str(&tail);
        } else {
            cur.push_str(&text);
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// True when a provider error means "the request does not fit".
fn is_context_overflow(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context window",
        "context length",
        "too many tokens",
        "maximum context",
        "input is too long",
        "prompt is too long",
        "request too large",
        "exceeds the maximum",
        "token limit",
        "reduce the length",
        "string too long",
    ]
    .iter()
    .any(|k| e.contains(k))
}

fn build_system(cwd: &Path, memory: &str) -> String {
    let body = r#"You are triagent, a terminal coding agent running on the user's machine.
Working directory: {cwd}
You can explore and edit files and run shell commands with the provided tools.

How to work:
- Prefer small, verifiable steps; run builds and tests when they help.
- Write complete files rather than fragments.
- Use bash for real commands, not for viewing files (use read_file, grep, glob).
- Never claim you ran, read or changed something you did not. If unsure, say so or check.
- When you make a mistake, fix it and keep going; no grovelling.

Response style:
- Write in prose. Use headings, bold or bullet lists only when the content is genuinely
  multi-part or the user asks; a short answer is a few sentences, not a template.
- Skip preambles and don't restate the request back.
- Ask at most one clarifying question, and only when it blocks progress; otherwise make the
  best assumption and state it in a clause.
- Be direct and honest: report failures plainly, state uncertainty instead of guessing, and
  push back with a reason rather than agreeing when something is wrong.
- Match the language the user writes in.

Safety:
- Do not help build weapons, malware, or content that sexualises minors; explain briefly and
  offer a safe alternative.
- For financial or legal questions give the facts and trade-offs rather than confident
  personal recommendations, and note that you are not a lawyer or financial advisor.
- For health topics give accurate information but do not diagnose; suggest a professional
  when it matters.
"#;
    let mut sys = body.replace("{cwd}", &cwd.display().to_string());
    if !memory.is_empty() {
        sys.push_str(&format!(
            "\nMemory (notes from earlier sessions):\n{memory}\n"
        ));
    }
    sys
}

async fn read_user_input() -> Result<String> {
    let read = tokio::task::spawn_blocking(|| {
        print!("\n{}", crate::tui::bold("❯ "));
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        Ok::<_, std::io::Error>(buf)
    });
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!();
            anyhow::bail!("exit")
        }
        r = read => Ok(r??),
    }
}

fn load_session(path: &Path) -> Result<Vec<Message>> {
    let raw = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<Message>(line) {
            out.push(m);
        }
    }
    if out.is_empty() {
        bail!("session file {} contains no messages", path.display());
    }
    Ok(out)
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn new_session_id() -> String {
    let ts = time::OffsetDateTime::now_utc();
    let ts = ts
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .replace([':', '-'], "_");
    format!("s-{ts}-{}", &uuid::Uuid::new_v4().to_string()[..8])
}

fn shorten(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

fn persist_theme(name: &str) {
    if let Ok(mut cfg) = config::Config::load_or_default() {
        cfg.ui.theme = name.to_string();
        let path = config::config_path();
        if let Ok(toml) = toml::to_string_pretty(&cfg) {
            let _ = std::fs::write(path, toml);
        }
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod context_tests {
    use super::*;

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            parts: vec![ContentPart::Text { text: text.into() }],
        }
    }

    #[test]
    fn context_window_known_models() {
        std::env::remove_var("TRIAGENT_CONTEXT_TOKENS");
        assert_eq!(context_window("gemini-3.8-flash-high"), 1_000_000);
        assert_eq!(context_window("claude-opus-4-6"), 200_000);
        assert_eq!(context_window("gpt-5-codex"), 400_000);
        assert_eq!(context_window("something-unknown"), 200_000);
    }

    #[test]
    fn detects_context_overflow_errors() {
        assert!(is_context_overflow(
            "API error 400: context_length_exceeded: your prompt is too long"
        ));
        assert!(is_context_overflow("Input is too long for this model"));
        assert!(is_context_overflow("request too large for model"));
        assert!(!is_context_overflow("401 unauthorized"));
        assert!(!is_context_overflow("rate limit exceeded"));
    }

    #[test]
    fn chunking_splits_on_budget() {
        let msgs: Vec<Message> = (0..10)
            .map(|i| msg(Role::User, &"x".repeat(1000 + i)))
            .collect();
        let chunks = chunk_messages(&msgs, 3000);
        assert!(
            chunks.len() >= 3,
            "expected several chunks, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.chars().count() <= 4200,
                "chunk too big: {}",
                c.chars().count()
            );
        }
    }

    #[test]
    fn chunking_truncates_single_huge_message() {
        let huge = msg(Role::User, &"a".repeat(50_000));
        let chunks = chunk_messages(&[huge], 10_000);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].chars().count() <= 11_000);
        assert!(chunks[0].contains("[truncated]"));
    }

    #[test]
    fn chars_of_counts_tool_parts() {
        let m = Message {
            role: Role::User,
            parts: vec![
                ContentPart::Text {
                    text: "hello".into(),
                },
                ContentPart::ToolResult {
                    tool_use_id: "a".into(),
                    content: "0123456789".into(),
                    is_error: false,
                },
            ],
        };
        assert!(chars_of(&[m]) >= 15);
    }

    #[test]
    fn flatten_renders_roles() {
        let out = msg_to_text(&msg(Role::Assistant, "hi there"));
        assert!(out.starts_with("assistant: hi there"));
    }
}
