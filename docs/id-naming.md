# How source ids are named

<!-- RELEASE GATE: the two-segment spellings and the alias read-side behaviour described on this page arrive with the release that carries the id-rename work (ADR-057). Until then the archive holds the one-word spellings and this page describes the standard the tool is converging on. Remove this comment when that release ships. -->

Every session chat-stasher archives is filed under a **source id**: the first word of the session id, naming the product the conversation came from. It is the same word you pass to `--harness`, the word `status` and the support tables print, and the name a page under [docs/sources/](sources/README.md) documents. This page states how that word is built, so a new source gets the right id the first time.

## The rule

A source id is **`<product>-<surface>`**:

- **product** — the vendor's own product name, lowercased, spaces turned into hyphens.
- **surface** — one of `cli`, `ide`, `app`, `web`, `bot`: where the conversation actually happened, as the **final** segment of the id.

Three refinements keep the vocabulary mechanical, so every id can be built from the rule rather than from a feel:

- **`-code`, `-agent`, `-harness` are product-name parts, never surface words.** `kimi-code` and `hermes-agent` end in those words and keep them: no `kimi-code-cli` is ever minted.
- **One store that mixes surfaces keeps the bare product name.** When a tool's sessions sit in one local store regardless of which front end produced them (one SQLite file a CLI, a TUI and an IDE all write to, for instance), the id stays the bare product and the per-session surface is recorded as provenance instead. A misleading suffix is worse than no suffix. `google-antigravity` is the worked example: one id across its CLI, IDE and desktop app, because the tool's own `/resume` makes them one conversation.
- **The capture channel never enters the id.** Whether a conversation arrived by live capture, by importing the platform's official export, or by pulling from an API, it is the same product on the same surface; the channel is recorded per record as provenance and fidelity. There are no `*-export` ids.

A bare product name that would accidentally parse as product + surface word (`claude-code` reads as product `claude` plus a nonexistent surface `code`) takes a surface suffix only as an explicit, separately-decided exception — `claude-code-cli` exists for exactly that reason and for the confusion its `claude` / `claude-code` adjacency would otherwise cause. New exceptions are decisions, not spellings you pick while writing the registry.

## Old ids and new ids

The first sources were named with one word: `codex`, `grok`, `chatgpt`, `claude`, `cursor` and so on. As sources multiplied, that one-word vocabulary stopped telling two different things apart, and one id could cover several genuinely different sources: `grok` in particular was doing duty for four — the Grok CLI's SQLite sessions, its native transcript files, grok.com web captures, and grok.com export imports.

New sources are named by the rule above, and the existing ones converge on it by adding the missing surface word:

| Old id | New id |
|---|---|
| `codex` | `codex-cli` |
| `claude-code` | `claude-code-cli` |
| `grok` (CLI sessions and transcript files) | `grok-cli` |
| `grok` (grok.com web captures and export imports) | `grok-web` |
| `chatgpt` | `chatgpt-web` |
| `claude` (web) | `claude-web` |
| `deepseek` (web) | `deepseek-web` |
| `gemini` (web) | `gemini-web` |
| `kimi` (web) | `kimi-web` |
| `perplexity` (web) | `perplexity-web` |
| `aider` | `aider-cli` |
| `crush` | `crush-cli` |
| `codebuff` | `codebuff-cli` |
| `cursor` | `cursor-ide` |
| `zed` | `zed-ide` |
| `continue` | `continue-ide` |
| `grok-bot`, `gemini-cli`, `kimi-code`, `deepseek-harness`, `github-copilot-cli`, `google-antigravity`, `hermes-agent`, `openclaw`, `opencode` | unchanged — already conformant |
| `letta` | unchanged — a deliberate keep, not a conformance call: no collision today, and renaming would re-key its audit HMAC scopes for zero disambiguation gain |

The **archive is append-only**, so this convergence never rewrites what exists: session ids and paths already archived stay exactly as they were written, forever, and remain first-class. No conversation ever gets a second archive entry because its id's vocabulary changed. Filters such as `--harness` accept the old and the new spelling of a source and mean the same thing by both, and lists, counts and search results group the two spellings under the new name — one source, one number, whichever spelling a given session's id carries.

## The `id_scheme` marker

Every record names the vocabulary of the id string it carries: an `id_scheme` of `2` says "this id is a two-segment spelling", and an absent marker means the older, one-word vocabulary. The marker belongs to the **id string itself, not to the binary that wrote the record**:

- an older installation writing the old spellings is correct, and its data needs no migration to stay first-class: absent means the old vocabulary, everywhere, and absence is never an error;
- a newer chat-stasher relaying a capture that an older browser extension captured keeps the capture's own vocabulary — it labels the id with what the id actually is, not with what version produced it;
- readers branch on the marker alone. Nothing anywhere guesses a record's meaning from the version that wrote it, so machines and extensions running different versions can share one destination indefinitely.

## Aliases are permanent

The old-to-new spellings are held in a small alias table shipped inside the CLI. The table is **append-only in the same sense as the archive**: entries are added, never changed or removed, and its entries are pinned by tests. The old spellings remain readable for the lifetime of the archive — there is no later step that retires them, and no decommissioning is ever planned. This is the accepted cost of never rewriting archived data: cheaper than redoing the archive, paid forever.

The one old id that splits in two, `grok`, is deliberately **not** a row in that table: a table row can only say "`grok` means X", and the truth is "`grok-cli` if these bytes came from the local harness, `grok-web` if they came from the web capture path". Each `grok` record is resolved from its own recorded provenance, so the split follows what the capture says about itself; a record that cannot be resolved is filed as `unknown` rather than guessed — same honesty rule as everywhere else in the tool.
