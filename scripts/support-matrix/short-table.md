**5+ platforms.** Local AI coding tools and web chats, archived the same way.

| Surface | Platform | Status | Last verified |
|---|---|---|---|
| Local | Claude Code | verified end-to-end (2026-09-25) | 2026-09-25 |
| Local | OpenAI Codex CLI | verified end-to-end (2026-09-25) | 2026-09-25 |
| Local | Gemini CLI | verified end-to-end (2026-09-25) | 2026-09-25 |
| Local | opencode | verified end-to-end (2026-09-25) | 2026-09-25 |
| Local | OpenClaw | supported | - |
| Local | Cursor | verified end-to-end (2026-09-25) | 2026-09-25 |
| Local | Grok Bot (desktop) | supported | - |
| Local | Grok (xAI CLI) | supported | - |
| Local | GitHub Copilot CLI | supported | - |
| Local | aider | supported | - |
| Local | crush | supported | - |
| Local | Zed | supported | - |
| Local | Continue | supported | - |
| Local | Kimi Code | supported | - |
| Web | deepseek | verified end-to-end (2026-09-24) | 2026-09-24 |
| Web | perplexity | experimental | - |
| Web | chatgpt | verified end-to-end (2026-09-24) | 2026-09-24 |
| Web | gemini | verified end-to-end (2026-09-24) | 2026-09-24 |
| Web | claude | verified end-to-end (2026-09-24) | 2026-09-24 |
| Web | kimi | experimental | - |
| Web | grok | verified end-to-end (2026-09-24) | 2026-09-24 |

Verified means a maintainer archived a real session end to end on their own machine. Formats change, so a date is recorded instead of a permanent check; a date older than 90 days is shown as needs re-check (DATE).

**Browsers.** Native host registration, per browser and per OS:

| Browser | macOS | Linux | Windows |
|---|---|---|---|
| Chrome | supported | supported | supported |
| Chromium | supported | supported | supported |
| Edge | supported | supported | supported |
| Brave | supported | supported | supported |
| Arc | supported | no native build | supported |
| Chrome Beta | unverified | unverified | unverified |
| Chrome Canary | unverified | unverified | unverified |
| Opera | unverified | unverified | unverified |
| Vivaldi | unverified | unverified | unverified |
| Firefox | supported | supported | supported |

`supported` is the promised tier: the discovery path, and on Windows the registry key, is documented and `install-native-host` registers it. `unverified` is the best-effort tier: registration is attempted and reported, never promised. `no native build` means the browser itself ships no build for that OS, so there is nothing to register. Firefox is carried as supported outside these Chromium-family tiers, on paths from Mozilla's own documentation. One registration serves every profile of a browser on a machine; the extension itself still needs loading once per profile.
