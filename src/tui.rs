//! Tiny terminal output helpers (ANSI, no TUI framework).

use std::io::{IsTerminal as _, Write};

const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn paint(color: &str, msg: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("{color}{msg}{RESET}")
    } else {
        msg.to_string()
    }
}

pub fn info(msg: &str) {
    println!("{}", paint(CYAN, msg));
}

pub fn ok(msg: &str) {
    println!("{}", paint(GREEN, msg));
}

pub fn warn(msg: &str) {
    println!("{}", paint(YELLOW, msg));
}

pub fn error(msg: &str) {
    eprintln!("{}", paint(RED, msg));
}

pub fn dim(msg: &str) -> String {
    paint(DIM, msg)
}

pub fn bold(msg: &str) -> String {
    paint(BOLD, msg)
}

/// Print streamed assistant text immediately (no trailing newline).
pub fn streamed(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

/// Print a tool-execution line.
pub fn tool_line(text: &str) {
    println!("{}", paint(DIM, text));
}

/// Synchronous stdin line read (used for permission prompts).
pub fn read_line_blocking(prompt: &str) -> std::io::Result<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}
