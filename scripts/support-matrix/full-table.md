### Local harnesses and agent platforms

| Harness | OS | Session path template | Format | Confidence | Status | Source |
|---|---|---|---|---|---|---|
| Claude Code | macOS | `~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl` | jsonl | source-confirmed | verified end-to-end (2026-09-25) | https://code.claude.com/docs/en/claude-directory |
| Claude Code | Linux | `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl` | jsonl | official-docs | verified end-to-end (2026-09-25) | https://code.claude.com/docs/en/claude-directory |
| Claude Code | Windows | `%USERPROFILE%\.claude\projects\<sanitized-cwd>\<session-uuid>.jsonl` | jsonl | official-docs | verified end-to-end (2026-09-25) | https://code.claude.com/docs/en/claude-directory |
| OpenAI Codex CLI | macOS | `~/.codex/sessions/` | jsonl / jsonl.zst | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/home-dir/src/lib.rs |
| OpenAI Codex CLI | Linux | `$HOME/.codex/sessions/` | jsonl / jsonl.zst | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/home-dir/src/lib.rs |
| OpenAI Codex CLI | Windows | `%USERPROFILE%\.codex\sessions\` | jsonl / jsonl.zst | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/home-dir/src/lib.rs |
| Gemini CLI | macOS | `~/.gemini/tmp/<projectId>/chats/` | json / jsonl | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/utils/paths.ts |
| Gemini CLI | Linux | `$HOME/.gemini/tmp/<projectId>/chats/` | json / jsonl | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/utils/paths.ts |
| Gemini CLI | Windows | `%USERPROFILE%\.gemini\tmp\<projectId>\chats\` | json / jsonl | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/google-gemini/gemini-cli/main/packages/core/src/utils/paths.ts |
| Google Antigravity (antigravity-cli) | macOS | `~/.gemini/antigravity-cli/brain/` | jsonl | measured-locally | verified end-to-end (2026-10-03) | - |
| Google Antigravity (antigravity-cli) | Linux | `$HOME/.gemini/antigravity-cli/brain/` | jsonl | unascertained | not supported | - |
| Google Antigravity (antigravity-cli) | Windows | `%USERPROFILE%\.gemini\antigravity-cli\brain\` | jsonl | unascertained | not supported | - |
| Google Antigravity (antigravity-ide) | macOS | `~/.gemini/antigravity-ide/brain/` | jsonl | measured-locally | verified end-to-end (2026-10-03) | - |
| Google Antigravity (antigravity-ide) | Linux | `$HOME/.gemini/antigravity-ide/brain/` | jsonl | unascertained | not supported | - |
| Google Antigravity (antigravity-ide) | Windows | `%USERPROFILE%\.gemini\antigravity-ide\brain\` | jsonl | unascertained | not supported | - |
| Google Antigravity (antigravity) | macOS | `~/.gemini/antigravity/brain/` | jsonl | measured-locally | verified end-to-end (2026-10-03) | - |
| Google Antigravity (antigravity) | Linux | `$HOME/.gemini/antigravity/brain/` | jsonl | unascertained | not supported | - |
| Google Antigravity (antigravity) | Windows | `%USERPROFILE%\.gemini\antigravity\brain\` | jsonl | unascertained | not supported | - |
| opencode | macOS | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/anomalyco/opencode/v1.18.4/packages/core/src/database/database.ts |
| opencode | Linux | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/anomalyco/opencode/v1.18.4/packages/core/src/database/database.ts |
| opencode | Windows | `$XDG_DATA_HOME/opencode/opencode.db` | sqlite | source-confirmed | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/anomalyco/opencode/v1.18.4/packages/core/src/database/database.ts |
| OpenClaw | macOS | `$HOME/.openclaw/agents` | sqlite | official-docs | supported | https://docs.openclaw.ai/concepts/session |
| OpenClaw | Linux | `$HOME/.openclaw/agents` | sqlite | official-docs | supported | https://docs.openclaw.ai/concepts/session |
| OpenClaw | Windows | `$HOME/.openclaw/agents` | sqlite | official-docs | supported | https://docs.openclaw.ai/concepts/session |
| Hermes Agent | macOS | `~/.hermes/state.db` | sqlite | official-docs | supported | https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/session-storage-recovery.md |
| Hermes Agent | Linux | `~/.hermes/state.db` | sqlite | official-docs | supported | https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/session-storage-recovery.md |
| Hermes Agent | Windows | `~/.hermes/state.db` | sqlite | official-docs | supported | https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/session-storage-recovery.md |
| Cursor | macOS | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` | sqlite | measured-locally | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/cursor/cursor/main/README.md |
| Cursor | Linux | `$XDG_CONFIG_HOME/Cursor/User/globalStorage/state.vscdb` | sqlite | community-claim-unverified | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/cursor/cursor/main/README.md |
| Cursor | Windows | `%APPDATA%\Cursor\User\globalStorage\state.vscdb` | sqlite | community-claim-unverified | verified end-to-end (2026-09-25) | https://raw.githubusercontent.com/cursor/cursor/main/README.md |
| Grok Bot (desktop) | macOS | `~/Library/Application Support/Grok Bot/` | json | measured-locally | supported | - |
| Grok Bot (desktop) | Linux | - | - | - | not supported | - |
| Grok Bot (desktop) | Windows | - | - | - | not supported | - |
| Grok (xAI CLI) | macOS | `~/.grok/sessions/session_search.sqlite` | sqlite | measured-locally | verified end-to-end (2026-10-03) | https://github.com/xai-org/grok-cli |
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
| Zed | macOS | `~/Library/Application Support/Zed/threads/threads.db` | sqlite | measured-locally | verified end-to-end (2026-10-03) | https://raw.githubusercontent.com/zed-industries/zed/main/crates/agent/src/db.rs |
| Zed | Linux | `$XDG_DATA_HOME/zed/threads/threads.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/zed-industries/zed/main/crates/agent/src/db.rs |
| Zed | Windows | `%LOCALAPPDATA%\Zed\threads\threads.db` | sqlite | source-confirmed | supported | https://raw.githubusercontent.com/zed-industries/zed/main/crates/agent/src/db.rs |
| Continue | macOS | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported | https://raw.githubusercontent.com/continuedev/continue/main/core/util/paths.ts |
| Continue | Linux | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported | https://raw.githubusercontent.com/continuedev/continue/main/core/util/paths.ts |
| Continue | Windows | `%USERPROFILE%\.continue\sessions\<id>.json + sessions.json` | json | source-confirmed | supported | https://raw.githubusercontent.com/continuedev/continue/main/core/util/paths.ts |
| Kimi Code | macOS | `~/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | source-confirmed | verified end-to-end (2026-10-03) | - |
| Kimi Code | Linux | `$HOME/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | unascertained | not supported | - |
| Kimi Code | Windows | `%USERPROFILE%\.kimi-code\sessions\<workspaceId>\<sessionId>\agents\main\wire.jsonl` | jsonl | unascertained | not supported | - |
| DeepSeek Harness | macOS | `~/.dsh/sessions/` | jsonl.zstd | measured-locally | supported | https://github.com/deepseek-ai/deepseek-harness |
| DeepSeek Harness | Linux | `$HOME/.dsh/sessions/` | jsonl.zstd | unascertained | not supported | https://github.com/deepseek-ai/deepseek-harness |
| DeepSeek Harness | Windows | `%USERPROFILE%\.dsh\sessions\` | jsonl.zstd | unascertained | not supported | https://github.com/deepseek-ai/deepseek-harness |

### Web AI chats (browser extension)

| Platform | Origins | Channel | Capture credibility | Status |
|---|---|---|---|---|
| deepseek | https://chat.deepseek.com | stable | from-source | verified end-to-end (2026-09-24) |
| perplexity | https://www.perplexity.ai | experimental | from-source | experimental |
| chatgpt | https://chatgpt.com, https://chat.openai.com | stable | from-source | verified end-to-end (2026-09-24) |
| gemini | https://gemini.google.com | stable | from-source | verified end-to-end (2026-09-24) |
| claude | https://claude.ai | stable | from-source | verified end-to-end (2026-09-24) |
| kimi | https://www.kimi.com | experimental | from-source | experimental |
| grok | https://grok.com | stable | from-source | verified end-to-end (2026-09-24) |

### Last verified, dev priority and known issues

| Surface | Tool or platform | Last verified | Dev priority | Known issue |
|---|---|---|---|---|
| Local | Claude Code | 2026-09-25 | high | on Windows the long-path directory hash is case-sensitive, so sessions can seem to disappear; the drive-letter and backslash sanitization of short paths is still unmeasured |
| Local | OpenAI Codex CLI | 2026-09-25 | normal | - |
| Local | Gemini CLI | 2026-09-25 | normal | the tool's own 30-day cleanup can delete chats in the source before a first archive runs; archive often |
| Local | Google Antigravity | 2026-10-03 | normal | - |
| Local | opencode | 2026-09-25 | normal | a one-line change still re-exports the whole session as a new full snapshot (git a908a00) |
| Agent platform | OpenClaw | - | normal | Cold transcript matching rules are documented in docs-dev/openclaw-scanner.md. |
| Agent platform | Hermes Agent | - | normal | - |
| Local | Cursor | 2026-09-25 | normal | - |
| Agent platform | Grok Bot (desktop) | - | low | local transcript replicas are partial and can contain sequence gaps; the archive does not claim completeness |
| Local | Grok (xAI CLI) | 2026-10-03 | low | the session_docs row preserves plain text and title but not speaker roles, turn boundaries, or per-message timestamps; the reader labels the speaker unknown and uses the session update time |
| Local | GitHub Copilot CLI | - | low | reader exports session-store.db (sessions+turns) per session; no real store has ever been measured (the schema is read from the v1.0.80 build artifacts), so conversation time is not claimed, and the per-session events.jsonl below session-state/ (turn-level cwd, event timestamps) is not read because its on-disk layout is unmeasured |
| Local | aider | - | low | no reader extractor yet: archived sessions render as raw view only (git 41db2bf) |
| Local | crush | - | low | no reader extractor yet: archived sessions render as raw view only (git 41db2bf) |
| Local | Zed | 2026-10-03 | low | image payloads, tool result contents, compaction summaries, per-message timestamps, and unrecognized content remain available in the raw archive and are not rendered |
| Local | Continue | - | low | no reader extractor yet: archived sessions render as raw view only (git 41db2bf) |
| Local | Kimi Code | 2026-10-03 | normal | Activity bounds are inferred and partial when unrecognised wire operation types occur. |
| Local | DeepSeek Harness | - | normal | measured on one machine against a developer preview (app 0.2.0-rc.2, session format 4); no session has been archived end to end yet |
| Web | deepseek | 2026-09-24 | normal | an account switch is not yet guarded: the new account list ids can land in the old run scope (see issue #4) |
| Web | perplexity | - | low | an account switch is not yet guarded: the new account list ids can land in the old run scope (see issue #4) |
| Web | chatgpt | 2026-09-24 | high | a workspace switch is not yet pinned to the run scope: the new workspace list ids can land in the old scope (see issue #4) |
| Web | gemini | 2026-09-24 | normal | a conversation needing more than 20 detail pages is refused and not archived in part (see docs-dev/privacy.md) |
| Web | claude | 2026-09-24 | normal | two accounts inside one organization are not yet distinguished by the organization check (see issue #4) |
| Web | kimi | - | low | an account switch is not yet guarded: the new account list ids can land in the old run scope (see issue #4) |
| Web | grok | 2026-09-24 | normal | the body endpoint is not paged; whether a long conversation comes back complete is unverified (see docs-dev/threat-model.md) |

### Browsers (native messaging host registration)

| Browser | OS | Status | Source |
|---|---|---|---|
| Chrome | macOS | supported | https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging |
| Chrome | Linux | supported | https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging |
| Chrome | Windows | supported | https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging |
| Chromium | macOS | supported | https://chromium.googlesource.com/chromium/src/+/HEAD/docs/user_data_dir.md |
| Chromium | Linux | supported | https://chromium.googlesource.com/chromium/src/+/HEAD/docs/user_data_dir.md |
| Chromium | Windows | supported | https://chromium.googlesource.com/chromium/src/+/ad587c3edba02a0c746651f6963e1b6e3763f1f7/chrome/browser/extensions/api/messaging/native_process_launcher_win.cc |
| Edge | macOS | supported | https://learn.microsoft.com/en-us/microsoft-edge/extensions/developer-guide/native-messaging |
| Edge | Linux | supported | https://learn.microsoft.com/en-us/microsoft-edge/extensions/developer-guide/native-messaging |
| Edge | Windows | supported | https://learn.microsoft.com/en-us/microsoft-edge/extensions/developer-guide/native-messaging |
| Brave | macOS | supported | https://github.com/gopasspw/gopass-jsonapi/blob/db70c6919e598d08190c9acfe1993a5c969156e8/internal/jsonapi/manifest/setup_windows.go |
| Brave | Linux | supported | https://github.com/gopasspw/gopass-jsonapi/blob/db70c6919e598d08190c9acfe1993a5c969156e8/internal/jsonapi/manifest/setup_windows.go |
| Brave | Windows | supported | https://github.com/gopasspw/gopass-jsonapi/blob/db70c6919e598d08190c9acfe1993a5c969156e8/internal/jsonapi/manifest/setup_windows.go |
| Arc | macOS | supported | https://github.com/keepassxreboot/keepassxc-browser/issues/1793 |
| Arc | Linux | no native build | https://arc.net/download |
| Arc | Windows | supported | - |
| Chrome Beta | macOS | unverified | https://chromium.googlesource.com/chromium/src/+/HEAD/docs/user_data_dir.md |
| Chrome Beta | Linux | unverified | https://chromium.googlesource.com/chromium/src/+/HEAD/docs/user_data_dir.md |
| Chrome Beta | Windows | unverified | https://chromium.googlesource.com/chromium/src/+/ad587c3edba02a0c746651f6963e1b6e3763f1f7/chrome/browser/extensions/api/messaging/native_process_launcher_win.cc |
| Chrome Canary | macOS | unverified | https://chromium.googlesource.com/chromium/src/+/HEAD/docs/user_data_dir.md |
| Chrome Canary | Linux | unverified | https://chromium.googlesource.com/chromium/src/+/HEAD/docs/user_data_dir.md |
| Chrome Canary | Windows | unverified | https://chromium.googlesource.com/chromium/src/+/ad587c3edba02a0c746651f6963e1b6e3763f1f7/chrome/browser/extensions/api/messaging/native_process_launcher_win.cc |
| Opera | macOS | unverified | https://forums.opera.com/topic/15735/porting-extension-from-chrome-macos-native-messaging |
| Opera | Linux | unverified | https://forums.opera.com/topic/15735/porting-extension-from-chrome-macos-native-messaging |
| Opera | Windows | unverified | https://forums.opera.com/topic/15735/porting-extension-from-chrome-macos-native-messaging |
| Vivaldi | macOS | unverified | https://github.com/vergenzt/TabFS/blob/master/install.sh |
| Vivaldi | Linux | unverified | https://github.com/vergenzt/TabFS/blob/master/install.sh |
| Vivaldi | Windows | unverified | https://github.com/vergenzt/TabFS/blob/master/install.sh |
| Firefox | macOS | supported | https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/Native_messaging |
| Firefox | Linux | supported | https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/Native_messaging |
| Firefox | Windows | supported | https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/Native_messaging |

`supported` means the path or route has a source and the scanner/extension will act on it; `verified end-to-end` additionally means a real session was archived on a real machine and the date is recorded. `unascertained` cells are not scanned and are rendered as `not supported`.

`Last verified` is the date a recorded run archived a real conversation end to end on a real machine; - means no run is recorded, and a date older than 90 days is shown as `needs re-check (DATE)`. Both `Dev priority` (`high` / `normal` / `low`, an editorial statement of where maintainer attention is) and `Known issue` (one short caveat, with a pointer to where it is tracked) are written by hand in the registry and the extension's platform table, which is why a change to them re-derives these tables too.

`supported` is the promised tier: the discovery path, and on Windows the registry key, is documented and `install-native-host` registers it. `unverified` is the best-effort tier: registration is attempted and reported, never promised. `no native build` means the browser itself ships no build for that OS, so there is nothing to register. Firefox is carried as supported outside these Chromium-family tiers, on paths from Mozilla's own documentation. One registration serves every profile of a browser on a machine; the extension itself still needs loading once per profile.
