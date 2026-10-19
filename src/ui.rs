//! Claude Code-style terminal UI: alternate screen, solid top bar,
//! scrollable transcript with card-style tool results, bordered input box,
//! inline permission prompts, and a slash-command menu (type `/` to open,
//! filter by typing, ↑/↓ to pick, Enter to run).

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::{mpsc, oneshot};
use unicode_width::UnicodeWidthStr;

use crate::agent::permissions::Answer;
use crate::theme::Theme;

const ESC: &str = "\x1b";
const CRLF: &str = "\r\n";
const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

// ---------------------------------------------------------------------------
// Agent-facing UI trait
// ---------------------------------------------------------------------------

pub trait Ui: Send + Sync {
    fn user(&self, text: &str);
    fn assistant_append(&self, text: &str);
    fn reasoning(&self, text: &str);
    fn tool_call(&self, name: &str, args: &str);
    fn tool_result(&self, content: &str, is_error: bool);
    fn info(&self, text: &str);
    fn diff(&self, text: &str);
    fn set_busy(&self, busy: bool);
    fn set_status(&self, left: String, right: String);
    fn set_theme(&self, name: String);
    fn show_picker(&self, title: &str, command_prefix: &str, items: Vec<(String, String)>);
    fn clear(&self);
}

// ---------------------------------------------------------------------------
// Slash-command catalog (also used by the runner's executor)
// ---------------------------------------------------------------------------

/// (name, description, needs-arguments)
pub const COMMANDS: &[(&str, &str, bool)] = &[
    // OpenCode commands
    ("/help", "show all commands", false),
    ("/compact", "summarize the conversation", false),
    ("/summarize", "alias for /compact", false),
    ("/undo", "undo the last turn", false),
    ("/redo", "redo an undone turn", false),
    ("/new", "start a fresh session", false),
    ("/init", "create TRIAGENT.md in this project", false),
    ("/clear", "reset conversation", false),
    ("/exit", "quit (session is saved)", false),
    ("/quit", "alias for /exit", false),
    ("/q", "alias for /exit", false),
    ("/connect", "add/login a provider", true),
    ("/login", "browser login for a provider", true),
    ("/logout", "remove a provider login", true),
    ("/model", "switch model", true),
    ("/models", "list available models", false),
    ("/provider", "switch provider", true),
    ("/details", "toggle full tool output", false),
    ("/editor", "compose a message in $EDITOR", false),
    ("/export", "export this session to markdown", false),
    ("/sessions", "list saved sessions", false),
    ("/resume", "load a saved session", true),
    ("/theme", "switch color theme", true),
    ("/themes", "list color themes", false),
    ("/thinking", "toggle reasoning display", false),
    ("/agents", "subagent info", false),
    ("/prompts", "list custom prompt commands", false),
    ("/skills", "list agent skills", false),
    ("/keybinds", "show keybindings", false),
    // Claude Code commands
    ("/status", "usage & quota info", false),
    ("/cost", "session tokens & cost", false),
    ("/usage", "alias for /cost", false),
    ("/permissions", "show permission mode", false),
    ("/memory", "edit the memory file", false),
    ("/doctor", "local health checks", false),
    ("/config", "show the config file", false),
    ("/pwd", "show working directory", false),
    ("/review", "review the current git diff", false),
    ("/rewind", "alias for /undo", false),
    ("/add-dir", "add an allowed directory", true),
    ("/todos", "show the task list", false),
    ("/output-style", "toggle detailed explanations", false),
    ("/security-review", "security review of git diff", false),
    ("/pr-comments", "review git diff for PR comments", false),
    ("/bug", "show info for a bug report", false),
    ("/release-notes", "draft release notes from git log", false),
    ("/upgrade", "check for updates", false),
];

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    User,
    Assistant,
    Reasoning,
    ToolCall,
    ToolResult,
    ToolError,
    Info,
    Diff,
}

#[derive(Clone, Debug)]
pub struct UiLine {
    pub kind: LineKind,
    pub text: String,
}

pub struct PendingQuestion {
    pub text: String,
    pub tx: Option<oneshot::Sender<Answer>>,
}

#[derive(Clone, Debug)]
pub struct MenuState {
    pub filter: String,
    pub selected: usize,
}

/// A dynamic picker (model list, etc.) shown above the input box.
#[derive(Clone, Debug)]
pub struct Picker {
    pub title: String,
    /// Prefix used to submit the choice, e.g. "/model ".
    pub command_prefix: String,
    pub items: Vec<(String, String)>,
    pub filter: String,
    pub selected: usize,
}

impl Picker {
    fn visible(&self) -> Vec<&(String, String)> {
        let f = self.filter.to_ascii_lowercase();
        self.items
            .iter()
            .filter(|(id, desc)| {
                f.is_empty()
                    || id.to_ascii_lowercase().contains(&f)
                    || desc.to_ascii_lowercase().contains(&f)
            })
            .collect()
    }
}

impl MenuState {
    fn matches(&self) -> Vec<usize> {
        COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (name, desc, _))| {
                let f = self.filter.to_ascii_lowercase();
                name.to_ascii_lowercase().contains(&f) || desc.to_ascii_lowercase().contains(&f)
            })
            .map(|(i, _)| i)
            .collect()
    }
}

pub struct AppState {
    pub lines: Vec<UiLine>,
    pub input: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub draft: Option<String>,
    pub left_status: String,
    pub right_status: String,
    pub busy: bool,
    pub question: Option<PendingQuestion>,
    pub scroll_offset: usize,
    pub menu: Option<MenuState>,
    pub picker: Option<Picker>,
    pub theme_name: String,
    pub exit: bool,
    pub dirty: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            draft: None,
            left_status: "triagent".into(),
            right_status: String::new(),
            busy: false,
            question: None,
            scroll_offset: 0,
            menu: None,
            picker: None,
            theme_name: "claude".into(),
            exit: false,
            dirty: true,
        }
    }
}

// ---------------------------------------------------------------------------
// TUI handle
// ---------------------------------------------------------------------------

pub struct Tui {
    state: Arc<Mutex<AppState>>,
    ask_tx: mpsc::UnboundedSender<(String, oneshot::Sender<Answer>)>,
    /// Receivers are taken by the runner once, right after startup.
    pub submit_rx: Option<mpsc::UnboundedReceiver<String>>,
    pub mode_rx: Option<mpsc::UnboundedReceiver<()>>,
    pub cancel_rx: Option<mpsc::UnboundedReceiver<()>>,
    exiting: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl Tui {
    pub fn start() -> Result<Self> {
        let state = Arc::new(Mutex::new(AppState::default()));
        let (submit_tx, submit_rx) = mpsc::unbounded_channel::<String>();
        let (ask_tx, mut ask_rx) = mpsc::unbounded_channel::<(String, oneshot::Sender<Answer>)>();
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<()>();
        let (mode_tx, mode_rx) = mpsc::unbounded_channel::<()>();
        let exiting = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));

        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        )?;

        // -- Input pump thread ------------------------------------------------
        {
            let state = state.clone();
            let exiting = exiting.clone();
            let paused = paused.clone();
            let submit_tx = submit_tx.clone();
            let cancel_tx = cancel_tx.clone();
            let mode_tx = mode_tx.clone();
            std::thread::spawn(move || {
                let mut last_cancel = Instant::now() - Duration::from_secs(10);
                while !exiting.load(Ordering::Relaxed) {
                    if paused.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    if !crossterm::event::poll(Duration::from_millis(100)).unwrap_or(false) {
                        continue;
                    }
                    let Ok(ev) = crossterm::event::read() else {
                        break;
                    };
                    let mut st = state.lock().unwrap();

                    // 1. Pending permission question?
                    if let Some(q) = st.question.take() {
                        let ans = match &ev {
                            Event::Key(k) if k.kind != KeyEventKind::Release => {
                                match (k.modifiers, k.code) {
                                    (_, KeyCode::Char('y'))
                                    | (_, KeyCode::Char('Y'))
                                    | (_, KeyCode::Enter) => Some(Answer::Yes),
                                    (_, KeyCode::Char('a')) | (_, KeyCode::Char('A')) => {
                                        Some(Answer::AllowAll)
                                    }
                                    (_, KeyCode::Char('n'))
                                    | (_, KeyCode::Char('N'))
                                    | (_, KeyCode::Esc)
                                    | (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                                        Some(Answer::No)
                                    }
                                    _ => None,
                                }
                            }
                            _ => None,
                        };
                        match ans {
                            Some(ans) => {
                                st.dirty = true;
                                if let Some(tx) = q.tx {
                                    let _ = tx.send(ans);
                                }
                            }
                            None => st.question = Some(q),
                        }
                        continue;
                    }

                    // 2. Picker open (models etc.)?
                    if st.picker.is_some() {
                        let submit = handle_picker_key(&mut st, &ev);
                        st.dirty = true;
                        if let Some(line) = submit {
                            drop(st);
                            let _ = submit_tx.send(line);
                        }
                        continue;
                    }

                    // 3. Slash-command menu open?
                    if st.menu.is_some() {
                        let submit = handle_menu_key(&mut st, &ev);
                        st.dirty = true;
                        if let Some(line) = submit {
                            drop(st);
                            let _ = submit_tx.send(line);
                        }
                        continue;
                    }

                    // 4. Normal editing / keys.
                    match &ev {
                        Event::Paste(text) => insert_text(&mut st, text),
                        Event::Key(k) if k.kind != KeyEventKind::Release => {
                            let mut submit: Option<String> = None;
                            match (k.modifiers, k.code) {
                                (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                                    let now = Instant::now();
                                    if now.duration_since(last_cancel) < Duration::from_millis(1500)
                                    {
                                        st.exit = true;
                                    } else {
                                        last_cancel = now;
                                        let _ = cancel_tx.send(());
                                    }
                                }
                                (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                                    if st.input.is_empty() {
                                        st.exit = true;
                                    }
                                }
                                (_, KeyCode::BackTab)
                                | (KeyModifiers::SHIFT, KeyCode::Tab)
                                | (_, KeyCode::Tab) => {
                                    let _ = mode_tx.send(());
                                }
                                (_, KeyCode::Char('?')) => {
                                    submit = Some("/help".to_string());
                                }
                                (_, KeyCode::Enter) => {
                                    let line = st.input.clone();
                                    if !line.trim().is_empty() {
                                        st.history.push(line.clone());
                                        if st.history.len() > 100 {
                                            st.history.remove(0);
                                        }
                                        submit = Some(line);
                                    }
                                    st.history_idx = None;
                                    st.draft = None;
                                    st.input.clear();
                                    st.cursor = 0;
                                }
                                (_, KeyCode::Char(c)) => {
                                    let opening_slash = c == '/' && st.input.is_empty();
                                    insert_text(&mut st, &c.to_string());
                                    if opening_slash {
                                        st.menu = Some(MenuState {
                                            filter: String::new(),
                                            selected: 0,
                                        });
                                    }
                                }
                                (_, KeyCode::Backspace) => {
                                    if st.cursor > 0 {
                                        let before: String =
                                            st.input.chars().take(st.cursor - 1).collect();
                                        let after: String =
                                            st.input.chars().skip(st.cursor).collect();
                                        st.input = format!("{before}{after}");
                                        st.cursor -= 1;
                                    }
                                }
                                (_, KeyCode::Delete) => {
                                    if st.cursor < st.input.chars().count() {
                                        let before: String =
                                            st.input.chars().take(st.cursor).collect();
                                        let after: String =
                                            st.input.chars().skip(st.cursor + 1).collect();
                                        st.input = format!("{before}{after}");
                                    }
                                }
                                (_, KeyCode::Left) => st.cursor = st.cursor.saturating_sub(1),
                                (_, KeyCode::Right) => {
                                    if st.cursor < st.input.chars().count() {
                                        st.cursor += 1;
                                    }
                                }
                                (_, KeyCode::Home) => st.cursor = 0,
                                (_, KeyCode::End) => st.cursor = st.input.chars().count(),
                                (_, KeyCode::Up) => {
                                    if !st.history.is_empty() {
                                        if st.history_idx.is_none() {
                                            st.draft = Some(st.input.clone());
                                        }
                                        let idx = st
                                            .history_idx
                                            .map(|i| i.saturating_sub(1))
                                            .unwrap_or(st.history.len() - 1);
                                        st.input = st.history.get(idx).cloned().unwrap_or_default();
                                        st.cursor = st.input.chars().count();
                                        st.history_idx = Some(idx);
                                    }
                                }
                                (_, KeyCode::Down) => {
                                    if let Some(i) = st.history_idx {
                                        let next = i + 1;
                                        if next >= st.history.len() {
                                            st.history_idx = None;
                                            st.input = st.draft.take().unwrap_or_default();
                                        } else {
                                            st.input =
                                                st.history.get(next).cloned().unwrap_or_default();
                                            st.history_idx = Some(next);
                                        }
                                        st.cursor = st.input.chars().count();
                                    }
                                }
                                (_, KeyCode::Esc) => {
                                    st.input.clear();
                                    st.cursor = 0;
                                }
                                (_, KeyCode::PageUp) => st.scroll_offset += 8,
                                (_, KeyCode::PageDown) => {
                                    st.scroll_offset = st.scroll_offset.saturating_sub(8);
                                }
                                _ => {}
                            }
                            st.dirty = true;
                            if let Some(line) = submit {
                                drop(st);
                                let _ = submit_tx.send(line);
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
                exiting.store(true, Ordering::Relaxed);
            });
        }

        // -- Render thread: plain ANSI full repaint ---------------------------
        {
            let state = state.clone();
            let exiting = exiting.clone();
            std::thread::spawn(move || {
                let mut stdout = std::io::stdout();
                while !exiting.load(Ordering::Relaxed) {
                    while let Ok((q, tx)) = ask_rx.try_recv() {
                        let mut st = state.lock().unwrap();
                        st.question = Some(PendingQuestion {
                            text: q,
                            tx: Some(tx),
                        });
                        st.dirty = true;
                    }
                    {
                        let mut snapshot = state.lock().unwrap();
                        if snapshot.dirty {
                            snapshot.dirty = false;
                            let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
                            let frame = render_to_ansi(&snapshot, w, h);
                            let _ = stdout.write_all(frame.as_bytes());
                            let _ = stdout.flush();
                        }
                    }
                    std::thread::sleep(Duration::from_millis(33));
                }
                let _ = crossterm::terminal::disable_raw_mode();
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::LeaveAlternateScreen,
                    crossterm::event::DisableBracketedPaste
                );
            });
        }

        Ok(Self {
            state,
            ask_tx,
            submit_rx: Some(submit_rx),
            mode_rx: Some(mode_rx),
            cancel_rx: Some(cancel_rx),
            exiting,
            paused,
        })
    }

    // -- suspension (for /memory, /login, /logout) ----------------------------

    pub fn suspend(&self) {
        self.paused.store(true, Ordering::Relaxed);
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableBracketedPaste
        );
    }

    pub fn resume(&self) {
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        );
        self.paused.store(false, Ordering::Relaxed);
        self.with(|st| st.dirty = true);
    }

    // -- state mutators used by the agent --------------------------------

    fn with<F: FnOnce(&mut AppState)>(&self, f: F) {
        let mut st = self.state.lock().unwrap();
        f(&mut st);
        st.dirty = true;
    }

    pub fn push(&self, kind: LineKind, text: String) {
        self.with(|st| {
            st.lines.push(UiLine { kind, text });
            if st.lines.len() > 5000 {
                st.lines.remove(0);
            }
        });
    }

    pub fn append_assistant(&self, text: &str) {
        self.with(|st| {
            if let Some(last) = st.lines.last_mut() {
                if last.kind == LineKind::Assistant {
                    last.text.push_str(text);
                    return;
                }
            }
            st.lines.push(UiLine {
                kind: LineKind::Assistant,
                text: text.to_string(),
            });
        });
    }

    pub fn set_status(&self, left: String, right: String) {
        self.with(|st| {
            st.left_status = left;
            st.right_status = right;
        });
    }

    pub fn set_busy(&self, busy: bool) {
        self.with(|st| st.busy = busy);
    }

    pub fn show_picker(&self, title: &str, command_prefix: &str, items: Vec<(String, String)>) {
        self.with(|st| {
            st.picker = Some(Picker {
                title: title.to_string(),
                command_prefix: command_prefix.to_string(),
                items,
                filter: String::new(),
                selected: 0,
            });
        });
    }

    pub fn set_theme(&self, name: String) {
        self.with(|st| st.theme_name = name);
    }

    pub fn clear_transcript(&self) {
        self.with(|st| st.lines.clear());
    }

    pub fn request_exit(&self) {
        self.exiting.store(true, Ordering::Relaxed);
    }

    pub fn ask_channel(&self) -> mpsc::UnboundedSender<(String, oneshot::Sender<Answer>)> {
        self.ask_tx.clone()
    }

    pub fn is_exiting(&self) -> bool {
        self.exiting.load(Ordering::Relaxed)
    }
}

/// Agent-facing adapter over the TUI.
pub struct TuiUi {
    pub tui: Tui,
}

impl TuiUi {
    pub fn new() -> Result<Self> {
        Ok(Self { tui: Tui::start()? })
    }
}

impl Ui for TuiUi {
    fn user(&self, text: &str) {
        self.tui.push(LineKind::User, text.to_string());
    }
    fn assistant_append(&self, text: &str) {
        self.tui.append_assistant(text);
    }
    fn reasoning(&self, text: &str) {
        self.tui.push(LineKind::Reasoning, text.to_string());
    }
    fn tool_call(&self, name: &str, args: &str) {
        self.tui.push(LineKind::ToolCall, format!("{name} {args}"));
    }
    fn tool_result(&self, content: &str, is_error: bool) {
        self.tui.push(
            if is_error {
                LineKind::ToolError
            } else {
                LineKind::ToolResult
            },
            content.to_string(),
        );
    }
    fn info(&self, text: &str) {
        self.tui.push(LineKind::Info, text.to_string());
    }
    fn diff(&self, text: &str) {
        self.tui.push(LineKind::Diff, text.to_string());
    }
    fn set_busy(&self, busy: bool) {
        self.tui.set_busy(busy);
    }
    fn set_status(&self, left: String, right: String) {
        self.tui.set_status(left, right);
    }
    fn set_theme(&self, name: String) {
        self.tui.set_theme(name);
    }
    fn show_picker(&self, title: &str, command_prefix: &str, items: Vec<(String, String)>) {
        self.tui.show_picker(title, command_prefix, items);
    }
    fn clear(&self) {
        self.tui.clear_transcript();
    }
}

// ---------------------------------------------------------------------------
// Menu key handling (runs on the input pump thread)
// ---------------------------------------------------------------------------

/// Returns Some(line) when a command should be submitted/committed.
fn handle_menu_key(st: &mut AppState, ev: &Event) -> Option<String> {
    let Event::Key(k) = ev else { return None };
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let menu = st.menu.as_mut().expect("menu open");
    let items = menu.matches();
    match (k.modifiers, k.code) {
        (_, KeyCode::Esc) => {
            st.menu = None;
            st.input.clear();
            st.cursor = 0;
            None
        }
        (_, KeyCode::Up) => {
            if !items.is_empty() {
                menu.selected = menu.selected.saturating_sub(1).min(items.len() - 1);
            }
            None
        }
        (_, KeyCode::Down) => {
            if !items.is_empty() {
                menu.selected = (menu.selected + 1).min(items.len() - 1);
            }
            None
        }
        (_, KeyCode::Enter) | (_, KeyCode::Tab) => {
            // If the user typed a command with arguments (a space past the
            // name), submit the whole line verbatim.
            if st.input[1..].contains(' ') {
                let line = st.input.clone();
                st.menu = None;
                st.input.clear();
                st.cursor = 0;
                return Some(line);
            }
            if items.is_empty() {
                return None;
            }
            let idx = items.get(menu.selected).copied().unwrap_or(0);
            let (name, _, needs_args) = COMMANDS[idx];
            st.menu = None;
            if needs_args {
                st.input = format!("{name} ");
                st.cursor = st.input.chars().count();
                None
            } else {
                st.input.clear();
                st.cursor = 0;
                Some(name.to_string())
            }
        }
        (_, KeyCode::Char(' ')) => {
            // Space accepts the command prefix and returns to normal typing.
            insert_text(st, " ");
            st.menu = None;
            None
        }
        (_, KeyCode::Backspace) => {
            if st.cursor > 1 {
                let before: String = st.input.chars().take(st.cursor - 1).collect();
                let after: String = st.input.chars().skip(st.cursor).collect();
                st.input = format!("{before}{after}");
                st.cursor -= 1;
                let filt = st.input[1..].to_string();
                if let Some(m) = st.menu.as_mut() {
                    m.filter = filt;
                    m.selected = 0;
                }
            } else if st.cursor == 1 {
                st.input.clear();
                st.cursor = 0;
                st.menu = None;
            }
            None
        }
        (_, KeyCode::Char(c)) => {
            let ch = c.to_string();
            insert_text(st, &ch);
            if let Some(m) = st.menu.as_mut() {
                m.filter = st.input[1..].to_string();
                m.selected = 0;
                if m.matches().is_empty() {
                    st.menu = None;
                }
            }
            None
        }
        _ => None,
    }
}

fn handle_picker_key(st: &mut AppState, ev: &Event) -> Option<String> {
    let Event::Key(k) = ev else { return None };
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let picker = st.picker.as_mut().expect("picker open");
    match (k.modifiers, k.code) {
        (_, KeyCode::Esc) => {
            st.picker = None;
            None
        }
        (_, KeyCode::Up) => {
            picker.selected = picker.selected.saturating_sub(1);
            None
        }
        (_, KeyCode::Down) => {
            let len = picker.visible().len();
            if len > 0 {
                picker.selected = (picker.selected + 1).min(len - 1);
            }
            None
        }
        (_, KeyCode::Enter) => {
            let visible: Vec<(String, String)> = picker.visible().into_iter().cloned().collect();
            let idx = picker.selected.min(visible.len().saturating_sub(1));
            let picked = visible.get(idx).cloned();
            let prefix = picker.command_prefix.clone();
            st.picker = None;
            picked.map(|(id, _)| format!("{prefix}{id}"))
        }
        (_, KeyCode::Backspace) => {
            picker.filter.pop();
            picker.selected = 0;
            None
        }
        (_, KeyCode::Char(c)) => {
            picker.filter.push(c);
            picker.selected = 0;
            None
        }
        _ => None,
    }
}

fn insert_text(st: &mut AppState, text: &str) {
    let before: String = st.input.chars().take(st.cursor).collect();
    let after: String = st.input.chars().skip(st.cursor).collect();
    st.input = format!("{before}{text}{after}");
    st.cursor += text.chars().count();
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_to_ansi(st: &AppState, width: u16, height: u16) -> String {
    let th = Theme::by_name(&st.theme_name);
    let dim = th.dim();
    let reset = th.reset();
    let w = width.saturating_sub(2).max(20) as usize;
    let mut out = String::with_capacity(8192);
    out.push_str(ESC);
    out.push_str("[2J");
    out.push_str(ESC);
    out.push_str("[H");
    out.push_str(ESC);
    out.push_str("[?25l");

    // ---- Top bar (solid background) ----------------------------------------
    let mut left = st.left_status.clone();
    if st.busy {
        let frame = SPIN[(Instant::now().elapsed().as_millis() / 100) as usize % SPIN.len()];
        left = format!("{left} {frame}");
    }
    let right = st.right_status.clone();
    let left_t = truncate_chars(&left, w.saturating_sub(right.chars().count() + 2));
    let pad = w.saturating_sub(left_t.chars().count() + right.chars().count());
    let mut bar = String::new();
    bar.push_str(&left_t);
    for _ in 0..pad {
        bar.push(' ');
    }
    bar.push_str(&right);
    out.push_str(&th.bar_bg());
    out.push_str(&th.bar_fg());
    out.push_str(&truncate_chars(&bar, w));
    out.push_str(&reset);
    out.push_str(CRLF);

    // ---- Layout budget (picker or command menu) -----------------------------
    let list_len = if let Some(p) = &st.picker {
        p.visible().len()
    } else if let Some(m) = &st.menu {
        m.matches().len()
    } else {
        0
    };
    let menu_h = if st.picker.is_some() || st.menu.is_some() {
        list_len.min(10) + 2
    } else {
        0
    };
    let input_h = 3; // bordered box rows
    let transcript_h = (height as usize)
        .saturating_sub(1) // top bar
        .saturating_sub(1) // divider
        .saturating_sub(menu_h)
        .saturating_sub(input_h);

    // ---- Transcript ---------------------------------------------------------
    let mut rows: Vec<(LineKind, String)> = Vec::new();
    for l in &st.lines {
        for wl in wrap(&l.text, w.saturating_sub(2)) {
            rows.push((l.kind, wl));
        }
    }
    let offset = st
        .scroll_offset
        .min(rows.len().saturating_sub(transcript_h));
    let end = rows.len().saturating_sub(offset);
    let start = end.saturating_sub(transcript_h);
    let shown = rows[start..end].len();
    for (kind, text) in &rows[start..end] {
        out.push_str(&paint_line(*kind, text, w, th));
        out.push_str(CRLF);
    }
    for _ in shown..transcript_h {
        out.push_str(CRLF);
    }

    // ---- Divider ------------------------------------------------------------
    out.push_str(&dim);
    for _ in 0..w {
        out.push('─');
    }
    out.push_str(&reset);
    out.push_str(CRLF);

    // ---- Picker / slash-command menu ----------------------------------------
    if let Some(picker) = &st.picker {
        let visible: Vec<(String, String)> = picker.visible().into_iter().cloned().collect();
        let title = format!("{} (↑/↓ · Enter · Esc)", picker.title);
        render_list(
            &mut out,
            &title,
            &visible,
            picker.selected,
            &picker.filter,
            w,
            th,
        );
    } else if let Some(menu) = &st.menu {
        let items: Vec<(String, String)> = menu
            .matches()
            .iter()
            .map(|&i| {
                let (n, d, _) = COMMANDS[i];
                (n.to_string(), d.to_string())
            })
            .collect();
        let mut title = String::from("slash commands (↑/↓ · Enter · Esc)");
        if !menu.filter.is_empty() {
            title.push_str(" · ");
            title.push_str(&menu.filter);
        }
        render_list(&mut out, &title, &items, menu.selected, &menu.filter, w, th);
    }

    // ---- Input box ----------------------------------------------------------
    let content = if let Some(q) = &st.question {
        format!("  ⚠ {} ", truncate_chars(&q.text, w.saturating_sub(16)))
    } else {
        st.input.clone()
    };

    // top border
    out.push_str(&dim);
    out.push('╭');
    for _ in 0..w.saturating_sub(2) {
        out.push('─');
    }
    out.push('╮');
    out.push_str(&reset);
    out.push_str(CRLF);
    // middle row
    out.push_str(&dim);
    out.push_str("│ ");
    out.push_str(&reset);
    if st.question.is_some() {
        out.push_str(&th.warn());
        out.push_str(&truncate_chars(&content, w.saturating_sub(4)));
        out.push_str(&reset);
    } else {
        out.push_str(&th.user());
        out.push_str("❯ ");
        out.push_str(&reset);
        let chars: Vec<char> = content.chars().collect();
        let cur = st.cursor.min(chars.len());
        let before: String = chars[..cur].iter().collect();
        let cursor_ch: String = chars
            .get(cur)
            .map(|c| c.to_string())
            .unwrap_or_else(|| " ".into());
        let after: String = chars
            .get(cur + 1..)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        out.push_str(&truncate_chars(&before, w.saturating_sub(8)));
        out.push_str(&th.bar_bg());
        out.push_str(&cursor_ch);
        out.push_str(&reset);
        out.push_str(&truncate_chars(&after, w.saturating_sub(8)));
    }
    out.push_str(&dim);
    out.push_str(" │");
    out.push_str(&reset);
    out.push_str(CRLF);
    // bottom border
    out.push_str(&dim);
    out.push('╰');
    for _ in 0..w.saturating_sub(2) {
        out.push('─');
    }
    out.push('╯');
    out.push_str(&reset);

    out
}

fn render_list(
    out: &mut String,
    title: &str,
    items: &[(String, String)],
    selected: usize,
    _filter: &str,
    w: usize,
    th: &Theme,
) {
    let dim = th.dim();
    let reset = th.reset();
    out.push_str(&dim);
    out.push('╭');
    out.push('─');
    out.push(' ');
    out.push_str(title);
    out.push(' ');
    let head = format!("╭─ {title} ").chars().count();
    let fill = w.saturating_sub(head + 1);
    for _ in 0..fill {
        out.push('─');
    }
    out.push('╮');
    out.push_str(&reset);
    out.push_str(CRLF);

    let start_idx = selected
        .saturating_sub(8)
        .min(items.len().saturating_sub(10));
    let end = (start_idx + 10).min(items.len());
    let (selbg, selfg) = th.selection();
    for (i, (id, desc)) in items
        .iter()
        .enumerate()
        .skip(start_idx)
        .take(end - start_idx)
    {
        let mut line = format!(" {id:<22} {desc}");
        line = truncate_chars(&line, w.saturating_sub(2));
        while line.chars().count() < w.saturating_sub(1) {
            line.push(' ');
        }
        if i == selected {
            out.push_str(&selbg);
            out.push_str(&selfg);
            out.push_str(&line);
            out.push_str(&reset);
        } else {
            out.push_str(&line);
        }
        out.push_str(CRLF);
    }
    out.push_str(&dim);
    out.push('╰');
    for _ in 0..w.saturating_sub(2) {
        out.push('─');
    }
    out.push('╯');
    out.push_str(&reset);
    out.push_str(CRLF);
}

fn paint_line(kind: LineKind, text: &str, w: usize, th: &Theme) -> String {
    let (code, prefix) = match kind {
        LineKind::User => (th.user(), "❯ "),
        LineKind::Assistant => (th.text(), ""),
        LineKind::Reasoning => (th.dim(), "  ⋮ "),
        LineKind::ToolCall => (th.tool(), "  ⚙ "),
        LineKind::ToolResult => (th.dim(), "  │ "),
        LineKind::ToolError => (th.err(), "  ⤷ "),
        LineKind::Info => (th.dim(), "  ℹ "),
        LineKind::Diff => {
            let code = if text.starts_with('+') {
                th.add()
            } else if text.starts_with('-') {
                th.del()
            } else {
                th.dim()
            };
            return format!(
                "{code}  {}{}",
                truncate_chars(text, w.saturating_sub(2)),
                th.reset()
            );
        }
    };
    let max = w.saturating_sub(prefix.chars().count());
    format!("{code}{prefix}{}{}", truncate_chars(text, max), th.reset())
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut current = String::new();
        let mut cur_w = 0usize;
        for word in raw.split_inclusive(' ') {
            let wl = UnicodeWidthStr::width(word);
            if cur_w + wl > width && cur_w > 0 {
                out.push(std::mem::take(&mut current));
                cur_w = 0;
            }
            current.push_str(word);
            cur_w += wl;
            if cur_w >= width {
                out.push(std::mem::take(&mut current));
                cur_w = 0;
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        if raw.is_empty() {
            out.push(String::new());
        }
    }
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.exiting.store(true, Ordering::Relaxed);
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableBracketedPaste
        );
        let _ = std::io::stdout().flush();
    }
}
