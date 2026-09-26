# Privacy and security

This is the short version: what chat-stasher does with your conversations, who can see what, and what it does **not** protect you from. The full, line-by-line records are [docs-dev/privacy.md](../docs-dev/privacy.md) (the privacy policy) and [docs-dev/threat-model.md](../docs-dev/threat-model.md) (the threat model). Every claim in those two points at the code that makes it true.

## What we receive

**Nothing.** There is no chat-stasher server, account, sign-up or sync service. The CLI and the extension contain no analytics, telemetry or crash reporting, and they send no usage pings. We cannot see your conversations, and we cannot delete them for you, because we never hold them.

## Where your conversations go

```
 your AI tools' own files ─┐
                           ├─► stage (this disk) ─► encrypted ─► your destination(s)
 extension outbox ─────────┘   (plaintext)                      (encrypted)
```

| Place | Encrypted? | Who can read it |
|---|---|---|
| The extension's outbox, in your browser profile, until the host confirms delivery. There is one outbox per install, so a browser profile you chat in without the extension has none | **No** | Anything running as your user |
| The stage folder on your disk | **No** | Anything running as your user |
| The optional local full-text index, if you built one | **No** | Anything running as your user |
| Your destination: local folder, SFTP or R2 | **Yes** | Only someone with the key file |
| The master key file, on your disk | Written readable only by you (`0600`, on macOS and Linux) | Anything running as your user |

Your storage provider holds encrypted objects only. It can still see how much you store, and when you back up.

The **full-text index** is the one place conversation text is written outside the archive. `chat-stasher index build` reads changed sessions, extracts user and assistant text, and stores it in a plaintext SQLite database under the operating-system cache directory (mode `0600`, in a `0700` directory, on macOS and Linux). It exists only so the dashboard's search can work; it is never uploaded, and `chat-stasher index clear` deletes it.

## The browser extension

- **Four permissions, no site access.** The extension asks for `nativeMessaging`, `storage`, `alarms` and `unlimitedStorage`. It has **no host permissions**, so it cannot reach any website by itself. None of the four raises an install-time warning beyond "communicate with cooperating native applications".
- **It runs only on the chat sites compiled into it, and nowhere else**: five platforms in the released build, seven in the development build.
- **Backfill requests are made from inside your own open, logged-in tab** of that platform, exactly as the page itself would make them. Where a platform needs a login token, the token is read from the page when needed, kept in memory only, and sent only to that platform's own endpoints.
- **Delivery goes to one place only:** the `chat-stasher` binary you registered on your own machine, which accepts nothing but this extension.

## The local dashboard

`chat-stasher ui` serves the archive on `127.0.0.1`, with a port picked by the system. That address is **not** a security boundary on its own: other programs on your machine can connect to it. So every launch gets a new random token, which appears only in the printed URL and is never logged or written to disk. The dashboard serves GET requests only, runs no JavaScript and loads nothing from the internet. It closes itself when idle. Treat its URL like a password while it is open.

## Keeping and deleting

- **The archive keeps everything, by design.** It exists so that history a tool deleted still survives. Each run adds a snapshot. Nothing in chat-stasher deletes from an archive, and there is no command to delete one conversation from it.
- **The only things chat-stasher deletes are its own copies, never yours**: the staged copy, and only after every destination proves it holds those exact bytes, and — when you ask with `chat-stasher index clear` — the local full-text index. Neither is an archive.
- **To delete everything,** delete the destination (the local folder, or the bucket or folder at your provider) and its key file. [install.md → Uninstalling](install.md#uninstalling) lists every local path.
- **Uninstalling the extension in one profile** deletes the captures that profile had not yet delivered, and leaves every other profile and browser delivering as before. Removing the host registration is the larger action: `chat-stasher install-native-host --uninstall` is machine-wide, and stops delivery for every browser and profile at once, so their captures stay in their own outboxes instead. [install.md → Before you remove the host](install.md#before-you-remove-the-host) gives the order that avoids losing a queued capture.

## What chat-stasher does not protect you from

- **Losing the key.** It is the only key. There is no recovery, escrow or reset, and no one can help.
- **A hostile program running as you.** It can read the outbox, the stage, your config and your key file. On a shared machine this is the main risk.
- **Other browser extensions.** Whether a second extension with broad permissions on a chat site can observe ours has not been tested. Treat it as potentially exposed.
- **Traffic analysis.** Your destination provider, and anyone watching your network, can see the size and timing of your backups.
- **No audit.** chat-stasher has not had a formal security assessment. "Not tested" is never written here as "not possible".

## The strongest setup available today

1. Keep your archive on a **local destination on an encrypted volume**. Then there is no provider to see anything.
2. Keep your **browser profile on an encrypted volume** too, because the extension's outbox lives there. Keep the host registered, so captures leave the outbox promptly.
3. Keep **key files somewhere other than the archive**, and back them up.
4. Run `chat-stasher verify` from time to time, rather than assuming the archive is intact.

## Reporting a vulnerability

Please report it privately, as described in [SECURITY.md](../SECURITY.md), not in a public issue.
