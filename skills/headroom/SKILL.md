---
name: headroom
description: Manage, configure, and integrate Headroom (local context compression layer for AI agents). Use when setting up token reduction, compressing tool outputs/logs/JSON/files, running the Headroom proxy, wrapping coding agents (Claude, Copilot, Cursor, Aider, etc.), configuring Headroom MCP tools, cross-agent memory, or failure learning (headroom learn).
---

# Headroom — Local Context Compression & Agent Optimization

Headroom is an open-source (Apache 2.0), 100% local context compression layer designed for AI agents and LLM applications. It compresses tool outputs, logs, RAG chunks, large files, and conversation turns before they reach the model—reducing input token spend by 20% to 55%+ (and 60%–95% on repetitive JSON) while preserving exact answer accuracy and provider KV-cache hits.

---

## When to Use This Skill

Activate this skill when:
- The user wants to reduce token usage and API costs in agent sessions (Claude Code, Antigravity, Cursor, Copilot, Aider, etc.).
- The user asks to install, configure, or run Headroom (`headroom proxy`, `headroom wrap`, `headroom learn`).
- Setting up the Headroom MCP server (`headroom_compress`, `headroom_retrieve`, `headroom_stats`).
- Integrating Headroom's inline compression into Python or TypeScript code.
- Diagnosing compression performance, token savings, or proxy connection issues.

---

## Mental Model & Core Concepts

1. **Local-First & Zero Leakage:** Runs entirely on the user's machine. No prompt text or file contents are sent to external compression APIs.
2. **Live-Zone Compression:** Only new turn content (fresh tool outputs, recent messages) is compressed. The prefix remains byte-identical so Anthropic and OpenAI KV prompt caches stay valid.
3. **Reversible Compression (CCR - Cached Content Retrieval):** Uncompressed originals are cached locally with a TTL. If the model needs full uncompressed details, it can call the `headroom_retrieve` tool.
4. **Specialized Routing Engines:**
   - **SmartCrusher:** Compresses large JSON payloads and database dumps, stripping uniform structural boilerplate while retaining outliers, boundaries, and errors.
   - **CodeCompressor:** AST-aware compressor for programming languages (Python, JS/TS, Go, Rust, Java, C++, etc.).
   - **Kompress-v2-base:** Small local neural model for compressing prose and unstructured text.
   - **Output Token Reducer:** Intercepts verbosity and trims model output tokens (preambles, redundant restatements).

---

## Installation & Setup

### 1. Install CLI & Core Engine
On Windows / macOS / Linux:

```powershell
# Recommended via uv (isolated environment with Python 3.13)
uv tool install --python 3.13 "headroom-ai[all]"

# Or via standard pip
pip install "headroom-ai[all]"

# Node.js / TypeScript SDK only (no CLI)
npm install headroom-ai
```

### 2. Verify Installation & Health
```powershell
headroom --version
headroom doctor              # Verifies routing, engines, and dependencies
headroom doctor --network    # Checks SSL certificates and upstream model connectivity
```

---

## Core Operational Modes

### Mode 1: Agent Wrapper (`headroom wrap`)
Wraps an agent CLI to automatically route its model traffic through a local compression proxy.

```powershell
# Wrap Claude Code
headroom wrap claude

# Wrap GitHub Copilot CLI with subscription mode
headroom copilot-auth login
headroom wrap copilot --subscription -- --model gpt-4o

# Wrap VS Code Copilot or VS Code Claude
headroom wrap vscode
headroom wrap vscode-claude

# Wrap other agents: aider, cursor, codex, grok, openhands, cline, continue
headroom wrap aider

# Undo wrapping
headroom unwrap claude
headroom unwrap vscode
```

### Mode 2: Local Drop-in Proxy (`headroom proxy`)
Starts an OpenAI- and Anthropic-compatible local proxy. Any SDK or client pointing to `http://localhost:8787/v1` automatically benefits from compression.

```powershell
# Start proxy on default port 8787
headroom proxy --port 8787

# Enable output token reduction & verbosity steering
$env:HEADROOM_OUTPUT_SHAPER = "1"
headroom proxy --port 8787
```

**Client Configuration:**
- OpenAI Base URL: `http://localhost:8787/v1`
- Anthropic Base URL: `http://localhost:8787`

### Mode 3: Model Context Protocol (MCP) Server
Allows any MCP-compliant agent (including Antigravity, Claude Desktop, Cursor) to directly invoke Headroom compression and retrieval tools.

**MCP Configuration (`mcp_config.json`):**
```json
{
  "mcpServers": {
    "headroom": {
      "command": "headroom",
      "args": ["mcp", "serve"]
    }
  }
}
```

**Exposed MCP Tools:**
- `headroom_compress`: Compresses arbitrary text, logs, or JSON payloads and returns the compressed string + retrieval token.
- `headroom_retrieve`: Retrieves the original full uncompressed text using a CCR retrieval token.
- `headroom_stats`: Returns current session token savings, compression ratios, and cache hits.

### Mode 4: Inline Code Compression (SDK)

**Python:**
```python
from headroom import compress
from openai import OpenAI

messages = [
    {"role": "system", "content": "You are a senior analyst."},
    {"role": "user", "content": "Analyze these logs: " + raw_log_dump}
]

# Compress before dispatching to LLM
compressed = compress(messages, model="gpt-4o")

client = OpenAI()
response = client.chat.completions.create(
    model="gpt-4o",
    messages=compressed.messages
)

print(f"Saved {compressed.tokens_saved} tokens ({compressed.compression_ratio:.1%})")
```

**TypeScript:**
```typescript
import { compress } from 'headroom-ai';

const result = await compress(messages, { model: 'gpt-4o' });
// result.messages contains the optimized payload
```

### Mode 5: Failure Mining & Lessons (`headroom learn`)
Inspects past failed agent sessions, identifies recurring errors or verbose habits, and writes persistent guidelines to markdown instruction files.

```powershell
# Dry run to inspect findings
headroom learn

# Apply lessons directly to project guidelines (GEMINI.md, AGENTS.md, or CLAUDE.md)
headroom learn --target AGENTS.md

# Tune verbosity habits
headroom learn --verbosity --apply
```

---

## Analytics & Diagnostics

- **Real-Time Savings Dashboard:**
  ```powershell
  headroom dashboard       # Opens live web UI showing token savings and compression curves
  ```
- **Measure Output Reduction:**
  ```powershell
  headroom output-savings
  ```
- **Benchmark Local Performance:**
  ```powershell
  headroom perf
  ```

---

## Troubleshooting & Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `HEADROOM_BEACON` | `on` | Set to `off` to disable anonymous telemetry. |
| `DO_NOT_TRACK` | `0` | Set to `1` to respect privacy standard. |
| `HEADROOM_OUTPUT_SHAPER` | `0` | Set to `1` to trim model response verbosity. |
| `HEADROOM_TLS_STRICT` | `1` | Set to `0` behind corporate MITM / Zscaler proxies. |
| `HEADROOM_CA_BUNDLE` | - | Path to custom corporate PEM certificate bundle. |
| `HF_HUB_OFFLINE` | `0` | Set to `1` if Hugging Face model downloads are blocked. |
