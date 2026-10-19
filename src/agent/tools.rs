//! Tool implementations. Every shell command runs in its own process group;
//! cancellation/timeout kills the whole group (SIGTERM, then SIGKILL) so no
//! descendant ever survives the CLI. Includes Claude Code-style features:
//! background shells, web fetch, and subagent tasks.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use super::permissions::{Answer, Asker, Mode};
use crate::providers::{ProviderId, ToolSpec};

pub const MAX_OUTPUT_CHARS: usize = 24_000;

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

#[derive(Clone)]
pub struct BgShell {
    pub pid: u32,
    pub out: Arc<Mutex<String>>,
    pub done: Arc<AtomicBool>,
}

pub struct ToolRuntime {
    cwd: PathBuf,
    add_dirs: Vec<PathBuf>,
    timeout: Duration,
    provider: ProviderId,
    model: String,
    bg: Arc<Mutex<HashMap<String, BgShell>>>,
}

impl ToolRuntime {
    pub fn set_provider_model(&mut self, provider: ProviderId, model: String) {
        self.provider = provider;
        self.model = model;
    }

    pub fn add_dir(&mut self, dir: PathBuf) {
        self.add_dirs.push(dir);
    }

    pub fn new(cwd: PathBuf, add_dirs: Vec<PathBuf>, provider: ProviderId, model: String) -> Self {
        Self {
            cwd,
            add_dirs,
            timeout: Duration::from_secs(120),
            provider,
            model,
            bg: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    fn allowed(&self, path: &Path) -> bool {
        path.starts_with(&self.cwd) || self.add_dirs.iter().any(|d| path.starts_with(d))
    }

    pub async fn execute(&self, name: &str, args: &Value, ask: &mut Asker) -> ToolOutput {
        // Plan mode gates anything that mutates the system.
        if ask.mode() == Mode::Plan && is_mutating_tool(name) {
            return ToolOutput {
                content: format!(
                    "Plan mode is active: {name} was NOT executed. \
                     When you have a complete plan, call exit_plan_mode with it."
                ),
                is_error: false,
            };
        }
        let result = match name {
            "bash" => self.bash(args, ask).await,
            "read_file" => self.read_file(args, ask).await,
            "write_file" => self.write_file(args, ask).await,
            "edit_file" => self.edit_file(args, ask).await,
            "glob" => self.glob(args).await,
            "grep" => self.grep(args).await,
            "list_dir" => self.list_dir(args).await,
            "todo" => self.todo(args).await,
            "webfetch" => self.webfetch(args).await,
            "task" => self.task(args, ask).await,
            "kill_shell" => self.kill_shell(args).await,
            "bash_output" => self.bash_output(args).await,
            other => Err(format!("unknown tool {other:?}")),
        };
        match result {
            Ok(content) => ToolOutput {
                content,
                is_error: false,
            },
            Err(e) => ToolOutput {
                content: e,
                is_error: true,
            },
        }
    }

    /// Run a shell command WITHOUT a permission prompt (the `!cmd` prefix).
    pub async fn shell_unprompted(&self, command: &str) -> ToolOutput {
        let timeout_ms = self.timeout.as_millis() as u64;
        match self.bash_exec(command, timeout_ms).await {
            Ok(c) => ToolOutput {
                content: c,
                is_error: false,
            },
            Err(e) => ToolOutput {
                content: e,
                is_error: true,
            },
        }
    }

    async fn bash_exec(&self, command: &str, timeout_ms: u64) -> Result<String, String> {
        let mut std_cmd = std::process::Command::new("/bin/bash");
        std_cmd
            .args(["-lc", command])
            .current_dir(&self.cwd)
            .process_group(0); // new process group for the whole tree

        let child = tokio::process::Command::from(std_cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("bash: spawn failed: {e}"))?;
        let pid = child.id().unwrap_or(0) as i32;
        register_child(pid);

        let fut = child.wait_with_output();
        let result = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                kill_group(Some(pid as u32));
                Err("interrupted (Ctrl-C)".to_string())
            }
            r = tokio::time::timeout(Duration::from_millis(timeout_ms), fut) => match r {
                Ok(Ok(out)) => {
                    let mut text = format!(
                        "[exit code: {}]\n{}",
                        out.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&out.stdout)
                    );
                    let err = String::from_utf8_lossy(&out.stderr);
                    if !err.is_empty() {
                        text.push_str("\n[stderr]\n");
                        text.push_str(&err);
                    }
                    Ok(truncate(&text, MAX_OUTPUT_CHARS))
                }
                Ok(Err(e)) => Err(format!("bash: {e}")),
                Err(_) => {
                    kill_group(Some(pid as u32));
                    Err(format!(
                        "command timed out after {timeout_ms}ms (process group killed)"
                    ))
                }
            },
        };
        unregister_child(pid);
        result
    }

    async fn bash(&self, args: &Value, ask: &mut Asker) -> Result<String, String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or("bash: missing 'command'")?
            .to_string();
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(self.timeout.as_millis() as u64)
            .min(600_000);
        let background = args
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if background {
            return self.bash_background(&command);
        }
        if ask.ask(&format!("run command: {command}")).await == Answer::No {
            return Err("command denied by user".into());
        }
        self.bash_exec(&command, timeout_ms).await
    }

    fn bash_background(&self, command: &str) -> Result<String, String> {
        let mut std_cmd = std::process::Command::new("/bin/bash");
        std_cmd
            .args(["-lc", command])
            .current_dir(&self.cwd)
            .process_group(0);
        let mut child = tokio::process::Command::from(std_cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("bash: spawn failed: {e}"))?;
        let pid = child.id().unwrap_or(0);
        register_child(pid as i32);

        let shell_id = format!("sh_{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let out = Arc::new(Mutex::new(String::new()));
        let done = Arc::new(AtomicBool::new(false));
        let out2 = out.clone();
        let done2 = done.clone();

        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stdout = child.stdout.take();
            let mut stderr = child.stderr.take();
            let mut buf = [0u8; 4096];
            let out = out2.clone();
            if let Some(mut s) = stdout.take() {
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                            let mut lock = out.lock().unwrap();
                            *lock = truncate(&format!("{}{}", lock, chunk), MAX_OUTPUT_CHARS);
                        }
                    }
                }
            }
            if let Some(mut s) = stderr.take() {
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                            let mut lock = out.lock().unwrap();
                            *lock = truncate(&format!("{}{}", lock, chunk), MAX_OUTPUT_CHARS);
                        }
                    }
                }
            }
            let status = child.wait().await;
            let mut lock = out.lock().unwrap();
            lock.push_str(&format!(
                "\n[exit code: {}]",
                status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)
            ));
            done2.store(true, Ordering::Relaxed);
            unregister_child(pid as i32);
        });

        let mut bg = self.bg.lock().unwrap();
        bg.insert(shell_id.clone(), BgShell { pid, out, done });
        Ok(format!(
            "started background shell {shell_id} (pid {pid}): {command}\n\
             use bash_output {{shell_id}} to poll, kill_shell {{shell_id}} to stop"
        ))
    }

    async fn kill_shell(&self, args: &Value) -> Result<String, String> {
        let shell_id = args
            .get("shell_id")
            .and_then(Value::as_str)
            .ok_or("kill_shell: missing 'shell_id'")?;
        let bg = {
            let mut map = self.bg.lock().unwrap();
            map.remove(shell_id)
        };
        match bg {
            Some(b) => {
                kill_group(Some(b.pid));
                b.done.store(true, Ordering::Relaxed);
                Ok(format!("killed {shell_id}"))
            }
            None => Err(format!("no such background shell: {shell_id}")),
        }
    }

    async fn bash_output(&self, args: &Value) -> Result<String, String> {
        let shell_id = args
            .get("shell_id")
            .and_then(Value::as_str)
            .ok_or("bash_output: missing 'shell_id'")?;
        let bg = self
            .bg
            .lock()
            .unwrap()
            .get(shell_id)
            .cloned()
            .ok_or_else(|| format!("no such background shell: {shell_id}"))?;
        let mut text = bg.out.lock().unwrap().clone();
        if !bg.done.load(Ordering::Relaxed) {
            text.push_str("\n[still running]");
        }
        Ok(truncate(&text, MAX_OUTPUT_CHARS))
    }

    async fn read_file(&self, args: &Value, ask: &mut Asker) -> Result<String, String> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or("read_file: missing 'path'")?;
        let path = self.resolve(raw);
        if !self.allowed(&path) && !ask_outside(&path, ask).await {
            return Err(format!("read denied: {raw}"));
        }
        if path.is_dir() {
            return Err(format!("{raw} is a directory; use list_dir"));
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("read_file: {e}"))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|l| l as usize);
        let lines: Vec<&str> = text.lines().collect();
        let start = offset.min(lines.len());
        let end = limit
            .map(|l| (start + l).min(lines.len()))
            .unwrap_or(lines.len());
        let slice = lines[start..end].join("\n");
        let note = if lines.len() > end {
            format!("\n[showing lines {}-{} of {}]", start + 1, end, lines.len())
        } else {
            String::new()
        };
        Ok(truncate(&format!("{slice}{note}"), MAX_OUTPUT_CHARS))
    }

    async fn write_file(&self, args: &Value, ask: &mut Asker) -> Result<String, String> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or("write_file: missing 'path'")?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or("write_file: missing 'content'")?;
        let path = self.resolve(raw);
        if !self.allowed(&path) && !ask_outside(&path, ask).await {
            return Err(format!("write denied: {raw}"));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("write_file: {e}"))?;
        }
        std::fs::write(&path, content).map_err(|e| format!("write_file: {e}"))?;
        Ok(format!("wrote {} ({} bytes)", raw, content.len()))
    }

    async fn edit_file(&self, args: &Value, ask: &mut Asker) -> Result<String, String> {
        let raw = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or("edit_file: missing 'path'")?;
        let old = args
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or("edit_file: missing 'old_string'")?;
        let new = args.get("new_string").and_then(Value::as_str).unwrap_or("");
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = self.resolve(raw);
        if !self.allowed(&path) && !ask_outside(&path, ask).await {
            return Err(format!("edit denied: {raw}"));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("edit_file: {e}"))?;
        if replace_all {
            let count = text.matches(old).count();
            if count == 0 {
                return Err("edit_file: old_string not found".into());
            }
            let updated = text.replace(old, new);
            std::fs::write(&path, updated).map_err(|e| format!("edit_file: {e}"))?;
            return Ok(format!("replaced {count} occurrence(s) in {raw}"));
        }
        let count = text.matches(old).count();
        if count == 0 {
            return Err("edit_file: old_string not found".into());
        }
        if count > 1 {
            return Err(format!(
                "edit_file: old_string matches {count} times; use a longer unique string or replace_all"
            ));
        }
        let updated = text.replacen(old, new, 1);
        std::fs::write(&path, updated).map_err(|e| format!("edit_file: {e}"))?;
        Ok(format!("edited {raw}"))
    }

    async fn glob(&self, args: &Value) -> Result<String, String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or("glob: missing 'pattern'")?;
        let base = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.cwd.clone());
        let matcher = globset::Glob::new(pattern)
            .map_err(|e| format!("glob: bad pattern: {e}"))?
            .compile_matcher();
        let mut hits = Vec::new();
        walk(&base, 0, 12, &mut |p| {
            if matcher.is_match(p.strip_prefix(&base).unwrap_or(p)) && hits.len() < 200 {
                hits.push(p.strip_prefix(&self.cwd).unwrap_or(p).display().to_string());
            }
        })
        .map_err(|e| format!("glob: {e}"))?;
        hits.sort();
        Ok(truncate(&hits.join("\n"), MAX_OUTPUT_CHARS))
    }

    async fn grep(&self, args: &Value) -> Result<String, String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or("grep: missing 'pattern'")?;
        let base = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.cwd.clone());
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| format!("grep: bad pattern: {e}"))?;
        let include = args
            .get("include")
            .and_then(Value::as_str)
            .map(globset::Glob::new);
        let include = match include {
            Some(Ok(g)) => Some(g.compile_matcher()),
            Some(Err(e)) => return Err(format!("grep: bad include: {e}")),
            None => None,
        };

        let mut out = String::new();
        let mut matches = 0usize;
        walk(&base, 0, 12, &mut |p| {
            if matches >= 500 || out.len() > MAX_OUTPUT_CHARS {
                return;
            }
            if p.is_dir() {
                return;
            }
            if let Some(m) = &include {
                if !m.is_match(p.strip_prefix(&base).unwrap_or(p)) {
                    return;
                }
            }
            let Ok(text) = std::fs::read_to_string(p) else {
                return;
            };
            let rel = p.strip_prefix(&self.cwd).unwrap_or(p);
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    matches += 1;
                    out.push_str(&format!("{}:{}: {line}\n", rel.display(), i + 1));
                    if matches >= 500 || out.len() > MAX_OUTPUT_CHARS {
                        return;
                    }
                }
            }
        })
        .map_err(|e| format!("grep: {e}"))?;
        if matches >= 500 {
            out.push_str("[more matches truncated]");
        }
        Ok(truncate(&out, MAX_OUTPUT_CHARS))
    }

    async fn list_dir(&self, args: &Value) -> Result<String, String> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = self.resolve(raw);
        let entries = std::fs::read_dir(&path).map_err(|e| format!("list_dir: {e}"))?;
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                match e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    true => format!("{name}/"),
                    false => name,
                }
            })
            .collect();
        names.sort();
        let mut out = names.join("\n");
        if names.len() >= 200 {
            out.push_str("\n[truncated]");
        }
        Ok(truncate(&out, MAX_OUTPUT_CHARS))
    }

    async fn todo(&self, args: &Value) -> Result<String, String> {
        let items = args
            .get("items")
            .and_then(Value::as_array)
            .ok_or("todo: missing 'items'")?;
        let mut out = String::from("todo list:\n");
        for (i, it) in items.iter().enumerate() {
            let content = it.get("content").and_then(Value::as_str).unwrap_or("");
            let status = it
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            out.push_str(&format!("  {}. [{status}] {content}\n", i + 1));
        }
        Ok(out)
    }

    async fn webfetch(&self, args: &Value) -> Result<String, String> {
        let raw = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or("webfetch: missing 'url'")?;
        let parsed = url::Url::parse(raw).map_err(|e| format!("webfetch: bad url: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("webfetch: only http/https URLs are allowed".into());
        }
        let client = crate::auth::http_client().map_err(|e| e.to_string())?;
        let resp = client
            .get(parsed)
            .send()
            .await
            .map_err(|e| format!("webfetch: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("webfetch: HTTP {status}"));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("webfetch: {e}"))?;
        if bytes.len() > 500_000 {
            return Err("webfetch: response too large (>500KB)".into());
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(truncate(&strip_html(&text), MAX_OUTPUT_CHARS))
    }

    /// Spawn a subagent: a fresh triagent process in print mode, same
    /// provider/model/cwd, permissions bypassed inside the subagent.
    async fn task(&self, args: &Value, ask: &mut Asker) -> Result<String, String> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or("task: missing 'prompt'")?;
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(prompt);
        if ask
            .ask(&format!("spawn subagent: {}", truncate(description, 80)))
            .await
            == Answer::No
        {
            return Err("subagent denied by user".into());
        }
        let depth = std::env::var("TRIAGENT_TASK_DEPTH")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        if depth >= 1 {
            return Err("nested subagents are disabled (depth limit 1)".into());
        }
        let exe = std::env::current_exe().map_err(|e| format!("task: {e}"))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["-p", prompt])
            .args(["--provider", self.provider.as_str()])
            .args(["--model", &self.model])
            .arg("--dangerously-skip-permissions")
            .current_dir(&self.cwd)
            .env("TRIAGENT_TASK_DEPTH", (depth + 1).to_string());
        for d in &self.add_dirs {
            cmd.arg("--add-dir").arg(d);
        }
        let out = tokio::time::timeout(Duration::from_secs(300), async {
            let o = tokio::process::Command::from(cmd)
                .output()
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(format!(
                "[exit {}]\n{}{}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ))
        })
        .await
        .map_err(|_| "subagent timed out after 300s".to_string())??;
        Ok(truncate(&out, MAX_OUTPUT_CHARS))
    }
}

fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "bash" | "write_file" | "edit_file" | "task" | "kill_shell"
    )
}

async fn ask_outside(path: &Path, ask: &mut Asker) -> bool {
    ask.ask(&format!("access outside workspace: {}", path.display()))
        .await
        == Answer::Yes
}

/// Recursive directory walk; does not follow symlinked dirs; skips `.git` and
/// hidden dirs; depth-limited.
fn walk<F: FnMut(&Path)>(dir: &Path, depth: usize, max_depth: usize, f: &mut F) -> Result<()> {
    if depth > max_depth || !dir.exists() {
        return Ok(());
    }
    if dir.is_dir() {
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
        if name.as_deref() == Some(".git") {
            return Ok(());
        }
        if depth > 0 && name.as_deref().map(|n| n.starts_with('.')).unwrap_or(false) {
            return Ok(()); // skip hidden dirs (except the walk root itself)
        }
        if dir.is_symlink() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.is_dir() {
                walk(&path, depth + 1, max_depth, f)?;
            } else {
                f(&path);
            }
        }
    } else {
        f(dir);
    }
    Ok(())
}

/// Kill an entire process group: SIGTERM, brief grace, SIGKILL. The direct
/// pid kill also covers the race where the child has not yet completed
/// setpgid (group kill would miss it).
pub fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(300));
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

// ---------------------------------------------------------------------------
// Active-child registry: on SIGHUP/SIGTERM (e.g. the terminal window closes)
// every still-running tool's process group is killed before we exit.
// ---------------------------------------------------------------------------

static ACTIVE_CHILDREN: std::sync::OnceLock<std::sync::Mutex<Vec<i32>>> =
    std::sync::OnceLock::new();

fn active() -> &'static std::sync::Mutex<Vec<i32>> {
    ACTIVE_CHILDREN.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

pub fn register_child(pid: i32) {
    if let Ok(mut list) = active().lock() {
        list.push(pid);
    }
}

pub fn unregister_child(pid: i32) {
    if let Ok(mut list) = active().lock() {
        list.retain(|p| *p != pid);
    }
}

pub fn kill_all_children() {
    if let Ok(list) = active().lock() {
        for pid in list.iter() {
            kill_group(Some(*pid as u32));
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max * 6 / 10).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(max * 4 / 10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head}\n…[truncated {} chars]…\n{tail}",
        s.chars().count() - max
    )
}

fn strip_html(text: &str) -> String {
    // Drop script/style blocks, then tags, then unescape basics.
    let no_blocks = regex::Regex::new(r"(?is)<(script|style)[^>]*>.*?</(script|style)>")
        .unwrap()
        .replace_all(text, " ");
    let no_tags = regex::Regex::new(r"<[^>]+>")
        .unwrap()
        .replace_all(&no_blocks, " ");
    let out = no_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let collapsed = regex::Regex::new(r"[ \t]{2,}")
        .unwrap()
        .replace_all(&out, " ");
    collapsed
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Tool schemas advertised to the models.
// ---------------------------------------------------------------------------

fn mk(name: &str, description: &str, schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
    }
}

fn string_prop(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

/// Tool set depends on permission mode (plan mode is read-only + exit_plan_mode)
/// and subagent depth (no nested tasks).
pub fn tool_specs(mode: Mode, task_depth: usize) -> Vec<ToolSpec> {
    if mode == Mode::Plan {
        let mut specs = vec![
            mk(
                "read_file",
                "Read a UTF-8 text file (optionally a line range).",
                json!({
                    "type": "object",
                    "properties": {
                        "path": string_prop(""),
                        "offset": {"type": "integer"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            mk(
                "glob",
                "Find files matching a glob pattern (e.g. src/**/*.rs).",
                json!({
                    "type": "object",
                    "properties": {"pattern": string_prop(""), "path": string_prop("")},
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            ),
            mk(
                "grep",
                "Search file contents with a regular expression.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": string_prop(""),
                        "path": string_prop(""),
                        "include": string_prop("optional filename glob"),
                        "case_sensitive": {"type": "boolean"}
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            ),
            mk(
                "list_dir",
                "List directory contents.",
                json!({
                    "type": "object",
                    "properties": {"path": string_prop("")},
                    "additionalProperties": false
                }),
            ),
            mk(
                "webfetch",
                "Fetch the text of a web page (http/https).",
                json!({
                    "type": "object",
                    "properties": {"url": string_prop("")},
                    "required": ["url"],
                    "additionalProperties": false
                }),
            ),
            mk(
                "exit_plan_mode",
                "Present the implementation plan for approval; on approval you may execute it.",
                json!({
                    "type": "object",
                    "properties": {"plan": string_prop("The plan, step by step")},
                    "required": ["plan"],
                    "additionalProperties": false
                }),
            ),
        ];
        if task_depth == 0 {
            specs.push(task_spec());
        }
        return specs;
    }

    let mut specs = vec![
        mk(
            "bash",
            "Run a shell command in the project directory. Long commands are killed after timeout_ms. Set run_in_background for long-running servers; read output with bash_output and stop with kill_shell.",
            json!({
                "type": "object",
                "properties": {
                    "command": string_prop(""),
                    "timeout_ms": {"type": "integer", "description": "max ms (default 120000)"},
                    "run_in_background": {"type": "boolean"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        mk(
            "read_file",
            "Read a UTF-8 text file (optionally a line range).",
            json!({
                "type": "object",
                "properties": {
                    "path": string_prop(""),
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        mk(
            "write_file",
            "Create or overwrite a file.",
            json!({
                "type": "object",
                "properties": {
                    "path": string_prop(""),
                    "content": string_prop("")
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        mk(
            "edit_file",
            "Replace an exact string in a file (must match exactly once unless replace_all).",
            json!({
                "type": "object",
                "properties": {
                    "path": string_prop(""),
                    "old_string": string_prop(""),
                    "new_string": string_prop(""),
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        ),
        mk(
            "glob",
            "Find files matching a glob pattern (e.g. src/**/*.rs).",
            json!({
                "type": "object",
                "properties": {"pattern": string_prop(""), "path": string_prop("")},
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        mk(
            "grep",
            "Search file contents with a regular expression.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": string_prop(""),
                    "path": string_prop(""),
                    "include": string_prop("optional filename glob"),
                    "case_sensitive": {"type": "boolean"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        mk(
            "list_dir",
            "List directory contents.",
            json!({
                "type": "object",
                "properties": {"path": string_prop("")},
                "additionalProperties": false
            }),
        ),
        mk(
            "todo",
            "Track a short task list.",
            json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": string_prop(""),
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        ),
        mk(
            "webfetch",
            "Fetch the text of a web page (http/https).",
            json!({
                "type": "object",
                "properties": {"url": string_prop("")},
                "required": ["url"],
                "additionalProperties": false
            }),
        ),
        mk(
            "bash_output",
            "Read the accumulated output of a background shell.",
            json!({
                "type": "object",
                "properties": {"shell_id": string_prop("")},
                "required": ["shell_id"],
                "additionalProperties": false
            }),
        ),
        mk(
            "kill_shell",
            "Stop a background shell (kills its process group).",
            json!({
                "type": "object",
                "properties": {"shell_id": string_prop("")},
                "required": ["shell_id"],
                "additionalProperties": false
            }),
        ),
    ];
    if task_depth == 0 {
        specs.push(task_spec());
    }
    specs
}

fn task_spec() -> ToolSpec {
    mk(
        "task",
        "Delegate a self-contained subtask to a fresh subagent (same model, print mode). Returns the subagent's final output.",
        json!({
            "type": "object",
            "properties": {
                "prompt": string_prop("complete self-contained instructions"),
                "description": string_prop("short summary")
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::permissions::{Asker, Mode};
    use serde_json::json;

    /// Process-signal tests share the global child registry and must not run
    /// concurrently with each other.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn runtime(dir: &std::path::Path) -> ToolRuntime {
        ToolRuntime::new(dir.to_path_buf(), vec![], ProviderId::Claude, "m".into())
    }

    #[tokio::test]
    async fn bash_runs_and_captures_output() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = runtime(tmp.path());
        let mut ask = Asker::new(false, Mode::Bypass, None);
        let out = rt
            .execute(
                "bash",
                &json!({"command": "echo hello; echo err >&2"}),
                &mut ask,
            )
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("err"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn bash_timeout_kills_process_group() {
        let _g = TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let rt = runtime(tmp.path());
        let mut ask = Asker::new(false, Mode::Bypass, None);
        let start = std::time::Instant::now();
        let out = rt
            .execute(
                "bash",
                &json!({"command": "sleep 300", "timeout_ms": 600}),
                &mut ask,
            )
            .await;
        assert!(out.is_error, "timed-out command must be an error");
        assert!(out.content.contains("timed out"));
        assert!(
            start.elapsed().as_secs() < 5,
            "kill must not wait for sleep"
        );
    }

    #[tokio::test]
    async fn non_interactive_default_mode_denies_bash() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = runtime(tmp.path());
        let mut ask = Asker::new(false, Mode::Default, None);
        let out = rt
            .execute("bash", &json!({"command": "echo nope"}), &mut ask)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("denied"));
    }

    #[tokio::test]
    async fn plan_mode_blocks_mutations() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = runtime(tmp.path());
        let mut ask = Asker::new(false, Mode::Plan, None);
        let out = rt
            .execute(
                "write_file",
                &json!({"path": "x.txt", "content": "hi"}),
                &mut ask,
            )
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("Plan mode"));
        assert!(!tmp.path().join("x.txt").exists());
        // read-only tools still work
        let out = rt
            .execute("list_dir", &json!({"path": "."}), &mut ask)
            .await;
        assert!(!out.is_error);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn background_shell_roundtrip() {
        let _g = TEST_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let rt = runtime(tmp.path());
        let mut ask = Asker::new(false, Mode::Bypass, None);
        let out = rt
            .execute(
                "bash",
                &json!({"command": "echo bg-hello; sleep 60", "run_in_background": true}),
                &mut ask,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let shell_id = out
            .content
            .split_whitespace()
            .nth(3)
            .expect("shell_id in output")
            .to_string();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let out = rt
            .execute("bash_output", &json!({"shell_id": shell_id}), &mut ask)
            .await;
        assert!(out.content.contains("bg-hello"));
        assert!(out.content.contains("still running"));
        let out = rt
            .execute("kill_shell", &json!({"shell_id": shell_id}), &mut ask)
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("killed"));
    }

    #[tokio::test]
    async fn edit_file_exact_match() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one two three").unwrap();
        let rt = runtime(tmp.path());
        let mut ask = Asker::new(false, Mode::Bypass, None);
        let out = rt
            .execute(
                "edit_file",
                &json!({"path": "a.txt", "old_string": "two", "new_string": "2"}),
                &mut ask,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one 2 three");
        std::fs::write(&f, "one two two").unwrap();
        let out = rt
            .execute(
                "edit_file",
                &json!({"path": "a.txt", "old_string": "two", "new_string": "2"}),
                &mut ask,
            )
            .await;
        assert!(out.is_error);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn kill_all_children_leaves_no_orphans() {
        let _g = TEST_LOCK.lock().unwrap();
        let mut std_cmd = std::process::Command::new("/bin/bash");
        // `exec` so the bash pid IS the sleeper (no grandchild to hide).
        std_cmd.args(["-lc", "exec sleep 300"]).process_group(0);
        let mut child = tokio::process::Command::from(std_cmd).spawn().unwrap();
        let pid = child.id().unwrap() as i32;
        register_child(pid);
        tokio::time::sleep(Duration::from_millis(300)).await;
        kill_all_children();
        let exited = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        assert!(exited.is_ok(), "child process survived kill_all_children");
        unregister_child(pid);
    }
}
