# Pull Letta API exports

`chat-stasher pull letta` reads `LETTA_API_KEY` from the environment. Supply
`--account-id` with the stable provider account ID independently of that key,
`--inbox` with a local inbox directory. Producer state defaults to the
application state directory; `--state` can select another private durable
directory outside repositories, inboxes and archive roots. Preserve that directory across restores: its random
identity salt makes the account's domain-separated HMAC stable across key
rotation. The raw account ID is never placed in envelope metadata or diagnostics.

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
--inbox`. Local publication and sealing do not prove archival: the per-agent
sequence cursor advances only when every configured destination proves it holds
all emitted sealed bytes. Until then, the command returns 3 and keeps the inbox
bundles and pending digest journal. A later `push` followed by another pull can
establish that proof for the earlier completed pass before producing fresh
captures. A newer incomplete pass does not undo an earlier proven cursor. No
bundle is retired by the producer.

Each pass re-reads complete conversation inventories, including below the
cursor. Edits append versions; complete inventories can append observations of
messages absent at source, with previous bundle digests, variant hashes, time,
and inventory evidence. These observations do not assert deletion or explain
retention. An incomplete enumeration or message walk establishes no absence.
A conversation missing from the listing gets an unavailable observation because
its access and retention cannot be established. Missing agent-window messages
likewise establish no absence. The local state is
a rebuildable cache: deleting its cursor records causes replay, but replacing
its identity salt changes scope and must be treated as a deliberate migration.

Requests are serial, paced by `--pace-ms` (default 1000), with 100-row pages,
finite timeouts, retry and page limits, and `--budget-seconds` (default 600).
429/503 honors delta-seconds or HTTP-date `Retry-After`; invalid headers use
bounded exponential backoff with jitter. Redirects are refused. Diagnostics show
counts only; no API key or response body is printed. Concurrent passes using the
same state directory serialize through a durable-state file lock; contention
returns an incomplete result rather than racing a cursor update.

Paced backfill inside `run-once` and local transcript collection remain separate
work. `run-once` does not invoke this producer.
