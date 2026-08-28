# Security Policy

`chat-stasher` archives your own conversation history to storage you control.
Before reporting, it may help to read [`docs/threat-model.md`](docs/threat-model.md),
which states plainly who can see what — including the parts we do not defend.

## Reporting a vulnerability

**Contact: `work@team.iopho.com`.** Mail that address directly rather than
opening a public issue — an issue is public from the moment you file it, and a
backup tool's weaknesses are worth a private window first.

Please avoid including your own conversation content, repository paths,
hostnames, or key material in the report. A minimal reproduction against
synthetic data is enough, and it is easier for us to act on.

GitHub private vulnerability reporting is not enabled yet; the mailbox above is
the channel.

When the channel is set up, we would like reports to include:

- what an attacker gains, stated as a capability ("can read X", "can cause Y to
  be silently dropped"), not just a code smell;
- the version or commit you tested;
- a reproduction that does not require your real archive.

## Response expectations

This is a personal project with a single maintainer. **We do not offer a
response-time SLA and will not promise one.** Concretely:

- There is no on-call rotation, no guaranteed acknowledgement window, and no
  guaranteed fix window.
- There is no security advisory mailing list.
- There is no bug bounty and no payment of any kind.

What we will do: acknowledge the report when the maintainer next works on the
project, say plainly whether we consider it in scope, and — if we fix it —
describe the fix in the commit and in `docs/threat-model.md` if it changes who
can see what. If we decide *not* to fix something, we will say so rather than
leave the report open indefinitely.

## Supported versions

| Version | Supported |
|---|---|
| `main` (unreleased) | Yes — this is the only line that receives fixes |
| `0.1.0` (`crates/chat-stasher/Cargo.toml:3`) | Version number only; no release tag exists in this repository |

There are no git tags and no published releases at the time of writing, so there
are no older versions to backport to. If you are running this, you are running a
checkout of `main`, and the fix path is "pull and rebuild".

## Scope

**In scope** — anything that breaks one of the four properties the project
actually claims (each is derived from code in `docs/threat-model.md`):

1. Conversation content reaching a network destination the user did not
   configure.
2. The extension capturing traffic outside its declared platform origins
   (`apps/extension/lib/contract.ts:53-125`).
3. The CLI writing to, or otherwise mutating, a harness's own session store,
   which is opened read-only (`crates/chat-stasher/src/sqlite_probe.rs:23-29`,
   `:1310-1313`).
4. `push` creating a snapshot that silently drops content it cannot account for
   (`crates/chat-stasher/src/main.rs:3886-3895`).

**Known and already documented, so not a new finding** — these are written up in
[`docs/threat-model.md`](docs/threat-model.md) and we are not currently defending
against them:

- The plaintext window in your browser's download directory.
- The master key file being readable by anything running as your user.
- Losing the key file, which makes the archive permanently unreadable.
- Backup metadata (size and timing) visible to a remote destination provider.

If you have found a *worse* consequence of one of those than what the threat
model describes, that is in scope — tell us what the threat model gets wrong.
