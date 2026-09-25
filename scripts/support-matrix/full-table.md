### Local AI coding tools

| Harness | OS | Session path template | Format | Confidence | Status | Source |
|---|---|---|---|---|---|---|
| Claude Code | macOS | `~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl` | jsonl | source-confirmed | supported | https://code.claude.com/docs/en/claude-directory |
| Claude Code | Linux | `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl` | jsonl | official-docs | supported | https://code.claude.com/docs/en/claude-directory |
| Claude Code | Windows | `%USERPROFILE%\.claude\projects\<sanitized-cwd>\<session-uuid>.jsonl` | jsonl | official-docs | supported | https://code.claude.com/docs/en/claude-directory |
| OpenAI Codex CLI | macOS | `~/.codex/sessions/` | jsonl / jsonl.zst | source-confirmed | supported | https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/home-dir/src/lib.rs |
| OpenAI Codex CLI | Linux | `$CODEX_HOME/sessions/` | jsonl / jsonl.zst | source-confirmed | supported | https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/home-dir/src/lib.rs |
| OpenAI Codex CLI | Windows | `%CODEX_HOME%\sessions\` | jsonl / jsonl.zst | source-confirmed | supported | https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/home-dir/src/lib.rs |
| Gemini CLI | macOS | `~/.gemini/tmp/<projectId>/chats/` | json / jsonl | source-confirmed | supported | https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/utils/paths.ts |
| Gemini CLI | Linux | `$HOME/.gemini/tmp/<projectId>/chats/` | json / jsonl | source-confirmed | supported | https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/utils/paths.ts |
| Gemini CLI | Windows | `%USERPROFILE%\.gemini\tmp\<projectId>\chats\` | json / jsonl | source-confirmed | supported | https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/utils/paths.ts |
| opencode | macOS | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/anomalyco/opencode/v1.18.4/packages/core/src/database/database.ts |
| opencode | Linux | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/anomalyco/opencode/v1.18.4/packages/core/src/database/database.ts |
| opencode | Windows | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/anomalyco/opencode/v1.18.4/packages/core/src/database/database.ts |
| Cursor | macOS | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` | sqlite | measured-locally | supported | https://raw.githubusercontent.com/cursor/cursor/main/README.md |
| Cursor | Linux | `$XDG_CONFIG_HOME/Cursor/User/globalStorage/state.vscdb` | sqlite | community-claim-unverified | uncertain (unverified) | https://raw.githubusercontent.com/cursor/cursor/main/README.md |
| Cursor | Windows | `%APPDATA%\Cursor\User\globalStorage\state.vscdb` | sqlite | community-claim-unverified | uncertain (unverified) | https://raw.githubusercontent.com/cursor/cursor/main/README.md |
| Grok (xAI CLI) | macOS | `~/.grok/sessions/session_search.sqlite` | sqlite | measured-locally | supported | https://github.com/xai-org/grok-cli |
| Grok (xAI CLI) | Linux | `$HOME/.grok/sessions/session_search.sqlite` | sqlite | unascertained | not supported | https://github.com/xai-org/grok-cli |
| Grok (xAI CLI) | Windows | `%USERPROFILE%\.grok\sessions\session_search.sqlite` | sqlite | unascertained | not supported | https://github.com/xai-org/grok-cli |
| GitHub Copilot CLI | macOS | `~/.copilot/` | sqlite + jsonl | source-confirmed | supported | https://raw.githubusercontent.com/github/copilot-cli/main/README.md |
| GitHub Copilot CLI | Linux | `~/.copilot/` | sqlite + jsonl | source-confirmed | supported | https://raw.githubusercontent.com/github/copilot-cli/main/README.md |
| GitHub Copilot CLI | Windows | `%USERPROFILE%\.copilot\` | sqlite + jsonl | source-confirmed | supported | https://raw.githubusercontent.com/github/copilot-cli/main/README.md |
| aider | macOS | `$CWD/.aider.chat.history.md` | markdown / txt / jsonl | source-confirmed | supported | https://raw.githubusercontent.com/Aider-AI/aider/main/aider/args.py |
| aider | Linux | `$CWD/.aider.chat.history.md` | markdown / txt / jsonl | source-confirmed | supported | https://raw.githubusercontent.com/Aider-AI/aider/main/aider/args.py |
| aider | Windows | `$CWD/.aider.chat.history.md` | markdown / txt / jsonl | source-confirmed | supported | https://raw.githubusercontent.com/Aider-AI/aider/main/aider/args.py |
| crush | macOS | `$CWD/.crush/crush.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/charmbracelet/crush/main/internal/db/connect.go |
| crush | Linux | `$CWD/.crush/crush.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/charmbracelet/crush/main/internal/db/connect.go |
| crush | Windows | `$CWD/.crush/crush.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/charmbracelet/crush/main/internal/db/connect.go |
| Zed | macOS | `~/Library/Application Support/Zed/threads/threads.db` | sqlite | community-claim-unverified | uncertain (unverified) | https://raw.githubusercontent.com/zed-industries/zed/main/crates/agent/src/db.rs |
| Zed | Linux | `$XDG_DATA_HOME/zed/threads/threads.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/zed-industries/zed/main/crates/agent/src/db.rs |
| Zed | Windows | `%LOCALAPPDATA%\Zed\threads\threads.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/zed-industries/zed/main/crates/agent/src/db.rs |
| Continue | macOS | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported | https://raw.githubusercontent.com/continuedev/continue/main/core/util/paths.ts |
| Continue | Linux | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported | https://raw.githubusercontent.com/continuedev/continue/main/core/util/paths.ts |
| Continue | Windows | `%USERPROFILE%\.continue\sessions\<id>.json + sessions.json` | json | source-confirmed | supported | https://raw.githubusercontent.com/continuedev/continue/main/core/util/paths.ts |
| Kimi Code | macOS | `~/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | source-confirmed | supported | — |
| Kimi Code | Linux | `$HOME/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | unascertained | not supported | — |
| Kimi Code | Windows | `%USERPROFILE%\.kimi-code\sessions\<workspaceId>\<sessionId>\agents\main\wire.jsonl` | jsonl | unascertained | not supported | — |

### Web AI chats (browser extension)

| Platform | Origins | Channel | Capture credibility | Status |
|---|---|---|---|---|
| deepseek | https://chat.deepseek.com | stable | from-source | supported |
| perplexity | https://www.perplexity.ai | experimental | from-source | experimental |
| chatgpt | https://chatgpt.com, https://chat.openai.com | stable | from-source | supported |
| gemini | https://gemini.google.com | stable | from-source | supported |
| claude | https://claude.ai | stable | from-source | supported |
| kimi | https://www.kimi.com | experimental | from-source | experimental |
| grok | https://grok.com | stable | from-source | supported |

`supported` means the path or route has a source and the scanner/extension will act on it; `verified end-to-end` additionally means a real session was archived on a real machine and the date is recorded. `unascertained` cells are not scanned and are rendered as `not supported`.
