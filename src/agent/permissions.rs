//! Permission prompting. Mirrors native agents: commands and writes outside
//! the workspace need approval in "default" mode; bypass mode never asks.
//! In TUI mode questions resolve through the UI's channel; in plain mode
//! they fall back to a blocking stdin read.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    AcceptEdits,
    Plan,
    Bypass,
}

impl Mode {
    pub fn from_str(s: &str) -> Mode {
        match s.trim().to_ascii_lowercase().as_str() {
            "accept-edits" | "auto" => Mode::AcceptEdits,
            "bypass" | "danger" => Mode::Bypass,
            "plan" => Mode::Plan,
            _ => Mode::Default,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::AcceptEdits => "accept-edits",
            Mode::Plan => "plan",
            Mode::Bypass => "bypass",
        }
    }

    /// Next mode in the Shift+Tab cycle (Claude Code order).
    pub fn next(&self) -> Mode {
        match self {
            Mode::Default => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Plan,
            Mode::Plan => Mode::Bypass,
            Mode::Bypass => Mode::Default,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    AllowAll,
}

/// Bridge to the TUI's inline permission prompt.
#[derive(Clone)]
pub struct AskBridge(pub mpsc::UnboundedSender<(String, oneshot::Sender<Answer>)>);

impl AskBridge {
    pub async fn ask(&self, question: &str) -> Answer {
        let (tx, rx) = oneshot::channel();
        if self.0.send((question.to_string(), tx)).is_err() {
            return Answer::No;
        }
        match tokio::time::timeout(Duration::from_secs(300), rx).await {
            Ok(Ok(ans)) => ans,
            _ => Answer::No,
        }
    }
}

pub struct Asker {
    pub interactive: bool,
    pub mode: Mode,
    allow_all: bool,
    bridge: Option<AskBridge>,
}

impl Asker {
    pub fn new(interactive: bool, mode: Mode, bridge: Option<AskBridge>) -> Self {
        Self {
            interactive,
            mode,
            allow_all: false,
            bridge,
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Ask for permission to run an action. Returns Yes/No ("a" grants Yes
    /// and remembers it for the session). Bypass mode never asks;
    /// non-interactive sessions deny by default (fail closed).
    pub async fn ask(&mut self, question: &str) -> Answer {
        if self.mode == Mode::Bypass || self.allow_all {
            return Answer::Yes;
        }
        if !self.interactive {
            eprintln!("  [denied] {question}");
            return Answer::No;
        }
        let answer = match &self.bridge {
            Some(b) => b.ask(question).await,
            None => blocking_ask(question).await,
        };
        if answer == Answer::AllowAll {
            self.allow_all = true;
            return Answer::Yes;
        }
        answer
    }
}

async fn blocking_ask(question: &str) -> Answer {
    let q = question.to_string();
    tokio::task::spawn_blocking(move || loop {
        print!("\x1b[33m  ⚠ {q} [y/n/A] \x1b[0m");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let line = crate::tui::read_line_blocking("").unwrap_or_default();
        match line.to_ascii_lowercase().as_str() {
            "y" | "yes" | "" => return Answer::Yes,
            "a" | "all" => return Answer::AllowAll,
            "n" | "no" => return Answer::No,
            _ => continue,
        }
    })
    .await
    .unwrap_or(Answer::No)
}
