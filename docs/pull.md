# Pull Letta API exports

`chat-stasher pull letta` reads `LETTA_API_KEY` from the environment. Supply
`--account-id` with the stable provider account ID independently of that key,
`--inbox` with a local inbox directory. Producer state defaults to the
application state directory; `--state` can select another private durable
directory outside repositories, inboxes and archive roots. Preserve that directory across restores: its random
identity salt makes the account's domain-separated HMAC stable across key
rotation. The raw account ID is never placed in envelope metadata or diagnostics.
When `/v1/environments` returns a connection, its documented `organizationId`
binds that observed organization to the existing tenant namespace. Both sides
of the binding are stored as domain-separated HMACs. Later use of a different
`--account-id` for that organization, or a different organization for that
supplied ID, is refused before publication. Existing tenant IDs do not change.
See the [provider's connection schema](https://docs.letta.com/api/typescript/resources/environments/methods/list).

This detects changes after the first observed binding; it cannot validate the
literal spelling of the first supplied account ID. An organization ID is not
assumed to be a user account ID. An empty connection list or a 403/404 on this
optional route leaves the supplied scope unverified, so a typo can still create
another namespace when identity is unavailable. Agent `identities` are
user-created identities, and `project_id` scopes a project; neither identifies
the authenticated account. Keep the supplied ID stable and verify it outside
this command. Ambiguous or malformed identity responses return 3.

```sh
chat-stasher pull letta --account-id "$LETTA_ACCOUNT_ID" \
  --inbox ./inbox --stage ./stage
```

The command lists agents and all conversations (`archive_status=all`), unions
agents referenced by conversations with local `~/.letta/agents/agent-*` IDs, and
retrieves each agent, including locally known hidden agents. It preserves every
message type and unknown field in JSONL API exports, orders messages by sequence,
and keeps distinct renderings of the same ID. A linked capture-order export
records the order in which each route served those variants. Conversation exports use
`full/api` for that API capture; agent history is a bounded window and declares
`partial` with the gap stated. Neither claim means all historical account data
or separately stored attachments have been captured. Local transcripts are a
separate required source that is not implemented by this command.

Bundles use the existing local inbox @3 `harness-file` contract with role
`api-export`, cloud surface, opaque tenant, agent container, and conversation
session. Agent-window-only messages use `agent-default-<agent-id>`. Full agent
and conversation metadata and the complete agent window are linked exports.
Observed parent references remain in those complete metadata objects; no parent
session is inferred from a parent agent.

Published bundles pass through `seal_payload`, the same sink as `ingest
--inbox`. Local publication and sealing do not prove archival: a per-agent pass
is proven only when every configured destination proves it holds all emitted
sealed bytes. Until then, the command returns 3 and keeps the inbox bundles and
pending digest journal. A later `push` followed by another pull can establish
that proof for the earlier completed pass before producing fresh captures.
While an agent's earlier pass remains unproven, no fresh bundles are published
for that agent; this keeps outstanding receipt state bounded to one pass.
Other agents can still progress. Existing accumulated receipt journals are
preserved until proof settles them; no required digest is discarded. No bundle
is retired by the producer.
Publication uses `.part` temporary entries, which ordinary inbox listing ignores.

Each pass deliberately re-reads complete conversation inventories to detect
edits and missing messages even at earlier sequence numbers. There is no
incremental sequence cursor or recorded high-water mark; `passes_proven` counts
agents whose completed pass obtained archive proof during this invocation.
The legacy SQLite `cursor` column is retained for state compatibility but cleared
and unused. Edits append versions; complete inventories can append observations
of messages absent at source, with previous pass bundle digests, variant hashes,
time, and inventory evidence. The journal keeps only the latest pass's bundle
references and cumulative variants for messages in the current inventory;
previous exported bundles and observations remain in the append-only archive.
These observations do not assert deletion or explain retention. An incomplete
enumeration or message walk establishes no absence. A conversation missing from
the listing gets an unavailable observation because its access and retention
cannot be established. Missing agent-window messages likewise establish no
absence. The local state is a rebuildable cache: deleting its pass records causes
replay, but replacing its identity salt changes scope and must be treated as a
deliberate migration.

Requests are serial, paced by `--pace-ms` (default 1000, about one request
per second), with pages sized by `--page-size` (default 100; the accepted
band is 100–200 rows, the provider rejects larger agent-history pages),
finite timeouts, retry and page limits, and `--budget-seconds` (default 600).
Pagination always stops on the existing page budget rather than looping
forever. 429/503 honors delta-seconds or HTTP-date `Retry-After`; invalid
headers use bounded exponential backoff with jitter. Redirects are refused.
Diagnostics show counts only; no API key or response body is printed.
Concurrent passes using the same state directory serialize through a
durable-state file lock; contention returns an incomplete result rather
than racing a receipt journal update.

## The scheduled pass

Declaring the producer in the config pulls it inside the hourly job, before
the pass's collect step — no separate command to remember:

```toml
[pull.letta]
account_id = "my-letta-account"
inbox = "~/stash/chat-stasher/letta-inbox"
api_key = "keychain:my-letta-key"
```

See [config.md](config.md) for every key. The pass pulls with the same one
pipeline as the command above: the same pacing, page-size band, `Retry-After`
handling and receipt journal, and an archive-proof rule with the same
purpose — except that the pass proves its receipts against the repository
**this pass pushes to** (its `--destination`/`--repo`/`--key-file` shape),
because that is where they are about to land.

`api_key` is the timer-reachable form: a literal in the config file, or a
`file:` / `env-file:` / `keychain:` reference — the same forms destination
options accept. Without it the pull reads `LETTA_API_KEY` from the
environment, which works for a terminal and **not** for a timer: a launchd
or systemd job sees no login shell, so every scheduled pass would exit 3 at
the credential check while the local archive cycle stays intact.
`schedule install`, `status`, `doctor` and the menu bar all name exactly
that trap with the same sentence when they see it. `chat-stasher pull
letta` (run by hand, with your environment) is unchanged.

What the pass does when the producer cannot finish reading is the point of
the integration. The local cycle (collect, push) still runs, so a later pass
can settle the producer's pending receipts — halting the whole pass instead
would deadlock the producer, whose proof needs an earlier push. But the pass
does not stop there being something left to read: it ends with exit 3
("did not finish reading") and   records the failed `pull-letta` step, so
`status` reports the failure — never a clean exit 0 that would read "no
messages". The unread remainder stays pending in the producer state exactly
as the standalone command leaves it. The record keeps the producer's
failure **beside** the local steps, not inside them: a pass whose collect
also failed keeps both facts, and `status` renders them as two lines — the
archive cycle's own health, and the producer's sentence. One corner worth
naming: with more than one destination, each destination's timer runs the
same declared producer, and they all share one producer state. Concurrent
passes serialize through that state's lock: the pass that wins it pulls the
producer and reports the producer's own result that hour; every other pass
**defers** — recorded as its own fact (`pull_letta_deferred` in the
run-state, surfaced in `status --json`), rendered by `status` as the
producer's line "Letta pull deferred …" beside the green archive line,
never as "did not finish reading" and never as an exit-3 failure, because
the winning pass owns that hour's pull and every pass's push moves the
shared stage. `schedule install` explains that line at the moment it first
creates the combination.

## What a round reads, publishes and costs

Every round re-verifies content — edit and disappearance detection compares
the fetched bytes, never a listing-row shortcut — and publication follows
the content:

- A conversation whose re-read content matches its last published round
  republishes **nothing**: no inbox bytes, `run-once` sees no new shards,
  and the unchanged hour is a genuine `NOOP` pass. The re-verification is a
  paced, resumable sweep, not a whole account per round: it starts where
  the previous round stopped and re-reads as many conversations as the
  budget paces (up to a few hundred per hour at the ~1/s pacing), so a
  caught-up round costs a bounded slice and every conversation is
  content-verified within a bounded number of rounds — whatever its
  listing row claims about it.
- A conversation that moved — new or edited messages, archived flag,
  content that changed while its row stayed still — republishes exactly
  its own bundles (its messages and conversation metadata) plus a new
  observation record when the change is an edit or a disappearance, on the
  sweep turn that re-reads it.
- A **large account completes across rounds**: the first pull persists its
  position, and a round whose budget runs out leaves the unwalked
  conversations pending — reported as progress (`pulled N of ~M`) in the
  pass line and in `status`, never as "no messages" — while the next round
  continues from the position instead of restarting. The sweep hand-off is
  the same mechanism: a steady round that cannot finish its slice resumes
  from the persisted position. The declared path's
  budget is capped at 45 minutes precisely so a round can never reach its
  own hourly period. The proof phase shares the round's deadline, so no
  phase of a pass borrows unbounded time from the next one.
- The round's archive proof opens each destination repository **once per
  round**, and asks about every pending receipt in one batch — an account
  with hundreds of publications costs the proof the same one repository
  open per destination, not one per bundle.
