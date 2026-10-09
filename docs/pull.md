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

Requests are serial, paced by `--pace-ms` (default 1000), with 100-row pages,
finite timeouts, retry and page limits, and `--budget-seconds` (default 600).
429/503 honors delta-seconds or HTTP-date `Retry-After`; invalid headers use
bounded exponential backoff with jitter. Redirects are refused. Diagnostics show
counts only; no API key or response body is printed. Concurrent passes using the
same state directory serialize through a durable-state file lock; contention
returns an incomplete result rather than racing a receipt journal update.

Paced backfill inside `run-once` and local transcript collection remain separate
work. `run-once` does not invoke this producer.
