# 5-minute quickstart

For a fresh Linux machine.

## 1. Install Rust (once)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
```

## 2. Build and install triagent

```bash
cd triagent            # this folder
cargo build --release
install -m755 target/release/triagent ~/.local/bin/triagent
```

## 3. Log in with a subscription

Pick the one you have:

```bash
triagent login claude
# or
triagent login chatgpt
# or
triagent login gemini
```

Complete the sign-in in the browser window that opens.

## 4. Run

```bash
triagent --provider gemini      # or claude / chatgpt
```

Type this as a first test:

```
explain what this folder is
```

Press `Ctrl+D` to quit.

## 5. Try your first skill

```
/ponytail write a minimal Python function that adds two numbers
```

## Cheatsheet

| | |
|---|---|
| `/` | command menu |
| `?` | help |
| `Shift+Tab` | permission mode |
| `Ctrl+C` ×2 | quit |
| `!ls` | run a shell command |
| `triagent -p "question"` | one-shot answer |
