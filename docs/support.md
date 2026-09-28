# What it supports

Every AI tool, chat site, browser, operating system and storage destination chat-stasher works with, and how sure we are about each one.

The three large tables below are **generated** from the registry that ships inside the CLI and from the extension's own platform list, by `scripts/gen-support-matrix.py`. They cannot drift from what the tool actually scans and registers. Do not edit them by hand: change the registry and regenerate.

## How to read the status

Two facts are kept apart on purpose: "we know where this tool keeps its sessions" and "a real session was archived end to end". A tool can change its storage in any release, so the first is never shown as the second.

| Status | Meaning |
|---|---|
| **verified end-to-end (DATE)** | A real session was archived on a real machine, and the date of that check is recorded. |
| **supported** | The path or route has a source, and the scanner or extension acts on it. End to end is not claimed. |
| **experimental** | A web platform enabled in the development build of the extension only. |
| **uncertain (unverified)** | A path exists only as an unconfirmed claim. It is not scanned unless you point `[harness_roots]` at it. |
| **not supported** | No path could be established for this system. It is not scanned, and the scanner reports *unknown*, never "0 sessions". |

The **Confidence** column says where a tool's path came from:

| Confidence | Source of the path |
|---|---|
| `source-confirmed` | Read from the tool's own source code |
| `official-docs` | The tool's official documentation |
| `measured-locally` | Observed on a real machine |
| `community-claim-unverified` | Reported by others, not confirmed |
| `unascertained` | Not established |

If a tool keeps its sessions somewhere else on your machine, set its path under `[harness_roots]` ([config.md](config.md#harness_roots)). A path you set is always scanned, whatever the table says.

## The support matrix

<!-- support-matrix:full:start -->
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
| Kimi Code | macOS | `~/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | source-confirmed | supported | - |
| Kimi Code | Linux | `$HOME/.kimi-code/sessions/<workspaceId>/<sessionId>/agents/main/wire.jsonl` | jsonl | unascertained | not supported | - |
| Kimi Code | Windows | `%USERPROFILE%\.kimi-code\sessions\<workspaceId>\<sessionId>\agents\main\wire.jsonl` | jsonl | unascertained | not supported | - |

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
| Arc | Windows | supported | https://github.com/chauncygu/collection-claude-code-source-code/blob/main/original-source-code/src/utils/claudeInChrome/common.ts |
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

`supported` is the promised tier: the discovery path, and on Windows the registry key, is documented and `install-native-host` registers it. `unverified` is the best-effort tier: registration is attempted and reported, never promised. `no native build` means the browser itself ships no build for that OS, so there is nothing to register. Firefox is carried as supported outside these Chromium-family tiers, on paths from Mozilla's own documentation. One registration serves every profile of a browser on a machine; the extension itself still needs loading once per profile.
<!-- support-matrix:full:end -->

## Operating systems (the CLI)

| System | Prebuilt binary | Install script | Hourly timer |
|---|---|---|---|
| macOS, Apple Silicon and Intel | Yes | Yes | launchd (`schedule install`) |
| Linux, x86-64 and arm64 | From 0.5.0: statically linked, any distribution | Yes, from 0.5.0 | systemd user timer (`schedule install --format systemd`) |
| Windows, x86-64 | From 0.5.0: `chat-stasher-windows-x86_64.exe` | No: download the file | None built in. Use Task Scheduler. |

From 0.5.0, `npm install -g chat-stasher` and `cargo install chat-stasher` also work. Before that, Linux and Windows build from source. See [install.md](install.md).

Most Linux and Windows tool paths come from each tool's source code or documentation, and have not yet been archived end to end on those systems.

## Destinations

| Destination | Status |
|---|---|
| Cloudflare R2 (`opendal:s3`) | Verified end to end (2026-09) |
| SFTP (`opendal:sftp`), for example a Hetzner Storage Box | In daily use on three machines |
| A local folder or disk | Supported |
| Other S3-compatible services (`opendal:s3`) | Same options as R2. Not tested. |
| A rustic REST server (`rest:…`) | Accepted by the config. Not verified. |

Setup for each is in [destinations.md](destinations.md).

## The browser extension

There is no such thing as "the extension". A real setup is several machines, each with several browsers, each with several profiles, and an extension lives in exactly one profile.

- **Where it runs:** the browsers in the table above. The **host** registration is per machine and covers every profile of each browser. The **extension** itself is loaded once per profile, so **install it in every profile you chat in**: a copy in one profile captures nothing in another, and each copy keeps its own queue and its own backfill progress. A profile you never open neither captures nor backfills.
- **What is shared, and what is not:** every install on this machine delivers into the **same stage**, under the same machine partition. What is not shared is a count: two profiles signed in to the same account may capture the same conversation, so the archive counts distinct conversations and raw copies separately, and never adds the per-install numbers together. One registration serves every profile; the extension itself does not.
- **How it is installed:** from each release's `chat-stasher-extension-X.Y.Z.zip`, with **Load unpacked**. It is not in any extension store yet. [install.md → The browser extension](install.md#the-browser-extension) has the steps.
- **Past conversations (backfill):** opt-in per platform, and not yet verified end to end on any platform. Each install reports for itself, so what one is still working through is never reported as what another has finished.

## Not supported yet

- **Restoring into a tool.** Sessions come out with `read`, `export` and the dashboard. Nothing writes them back into a tool's own folder.
- **Deleting a conversation from an archive.** The archive is append-only by design.
- **Extension stores.** The extension is loaded from a zip.
- **A built-in timer on Windows.**
