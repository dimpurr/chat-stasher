<!-- BEGIN GENERATED -->
# Grok Bot (desktop)

| Field | Value |
|---|---|
| Id | `grok-bot` |
| Surface | desktop app |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/Library/Application Support/Grok Bot/` | json | measured-locally | supported |
| Linux | - | - | - | not supported |
| Windows | - | - | - | not supported |
<!-- END GENERATED -->

## What it is

Grok Bot is the Grok desktop app for agent conversations: a thin client whose Bots run on a remote computer, with the conversation itself held on the vendor's servers. The machine running the app keeps only a local synchronization cache of each conversation — a bounded window of the server-side transcript, refreshed while the app is running. This tool reads that cache read-only and never launches or changes the app. Only macOS is supported, because that is the only platform with a measured cache location so far.

## What we collect

Every registered conversation's replica is one per-agent stream of records. On disk each replica is a `.blob` file whose name is the unpadded lowercase base32 of its state key, so the reader decodes file names and matches the keys ending in the replica suffix rather than looking for any literal file name; a roster blob supplies the agent names. Seven record kinds exist, and the reader maps each to messages: `message` is the human-to-agent transcript (speaker from the record's own role field, text from its content, or the rich text only when the content is empty); `notice` is a system banner; `send-message` is the bot-to-person direction, rendered as an assistant turn whose question widgets, approval gates, machine-action requests and box instructions become tool-call blocks, and whose delivered files and images become attachment references; `user-attachment` is the person's upload; `voice-call` is one spoken call expanded into its turns and nudges in the order the call stamped them; `event` is automation bookkeeping that is counted but renders nothing; and `at` is a sync cursor, skipped with zero counts because it is a record of nothing said. A record that names a credential exchange renders no message at all and contributes nothing to search, not even its own labels — the raw record stays archived untouched. A sub-agent that an approval delegated stays inside its parent agent's conversation as that gate's own tool call, so the one conversation stays whole and no child session is invented.

Collect merges each scan with everything observed before into one per-agent union: an identical repeat is nothing new, and a position re-observed with changed content — the app rewrites a row while streaming — is kept as an additional variant of that position rather than an overwrite. Each delivered shard carries one `_chat_stasher` metadata line with the union's own measurement: the missing sequence positions, whether the replica is partial, and the observed bounds.

## Coverage and completeness

The cache is a window, never the whole history, and the archive does not pretend otherwise. Measurements on real installations bound it in four ways. A conversation keeps roughly the last 200 records and at most 768 KiB, and positions that fall out of the window are gone from the local cache for good — a long conversation ends up with its whole head missing, which the reader reports as a gap run that starts at position one. A replica that has been inactive for about seven days is dropped by the app. Roughly one in seven of the positions inside the window is never downloaded at all: those holes are interior, one position wide, sit exactly between user-visible rows, and never fill in later observations — every indication is that they are the tool-call and branch rows the client deliberately does not mirror, but local data cannot prove what a hole held, so the archive counts them as known-unknowns. And the cache only moves while the app is running and signed in: while it runs, writes land within seconds whether or not the window is frontmost, and with it closed nothing is written at all, so coverage is the union of the periods the app actually ran between collections. What a single read does capture is essentially everything user-visible the app itself shows: measurements found no eviction, no truncation of stored rows, and no feature the app offers to pull the missing history down into the cache.

## Quirks

One sequence position can arrive more than once: the app rewrites a row while streaming, and the append-only union keeps every observed variant. The derived reading renders one message per position — the last variant marked complete, or the last variant when none is — while the raw archive keeps them all. Speaker labels ride on routing fields the reader deliberately ignores: a channel sender or a connector name never decides who said something, so some records render with the speaker unknown rather than guessed.

Re-collection has one behavior worth knowing before you use it. The delivered-set cursor is keyed by the source file's path. Point `[harness_roots]` at a copy of the cache — a backup, a rescue feed — and the next ordinary collect reads the real path again, finds a cursor no earlier pass wrote a digest set under, and re-delivers every row it can prove it read: a full duplicate set of rows, once per distinct path you ever collected from. Nothing is lost and nothing pre-existing is overwritten — the shards are append-only and byte-identical repeats change nothing — but a session's derived view then shows its duplicated rows twice, because the replay differs in its metadata line from the bytes already stored. If you must test against a copy, expect that one-time full re-delivery afterwards; do not route a rescue through a new path as a habit.

## Known issues

Local transcript replicas are partial and can contain sequence gaps; the archive does not claim completeness — that is the registry's standing caveat for this harness, and the reader repeats it on every session as `partial_replica` with the measured gap runs.

No export path exists anywhere in the shipped app: no menu, setting, IPC channel or request takes a conversation out of it, on any of those four surfaces, and neither the app vendor's nor xAI's public documentation describes a takeout that covers Grok Bot conversations. The app's own FAQ says its account and data settings follow the desktop app's authentication system, not the grok.com one, so a data-portability request is a matter for the account provider ([the FAQ](https://docs.x.ai/grok-bot/faq) is the public record). Until any of that changes, local periodic collection is the only automated path, and it inherits every bound in the section above.

## How to check it yourself

Run `chat-stasher doctor` — on macOS it reports the harness as found or not, with its session count. Run a normal `chat-stasher run-once` and then `chat-stasher read` on one of the staged `grok-bot` sessions: the derived view renders the messages like the page describes, and the session's metadata states whether the replica was partial and which position runs are missing. `chat-stasher verify` then checks that what was staged is what the destination holds, and `chat-stasher search --text` finds text from a conversation you know is in the window. To try the reader without touching a live cache, first `cp -Rp` the persistence directory to a scratch location and point `[harness_roots]` at the copy for the test — remembering the re-delivery behavior above so a later ordinary collect does not surprise you. None of these steps writes into the app's own files: every read the tool makes is read-only.

## Changelog

2026-10-03 — first shipped: the registry entry, the macOS scanner and the collect union, with per-agent state unions and measured sequence gaps.
2026-10-11 — the derived reader renders all seven record kinds, including the tool-shaped ones (widgets, approvals, machine-action requests) and spoken calls.
2026-10-12 — sub-agent delegations render inside their parent conversation, and credential-exchange records are withheld from every rendered and indexed surface while staying archived.
