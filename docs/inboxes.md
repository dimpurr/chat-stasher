# Remote inboxes

A remote inbox lets a machine that cannot run chat-stasher (a cloud agent
sandbox, a CI job) hand a transcript to one that can. The producer holds only a
public recipient and a posting key: it encrypts the capture, signs it and drops
one opaque object into the inbox. The archive machine pulls the objects, checks
them, seals them into its stage and removes an object only after an independent
read of the archive destination has found the bytes there.

The inbox is a hand-off, not an archive. Nothing here changes append-only
archiving: a refused or unproven object stays in the inbox until a person looks
at it.

## Declare an inbox (archive machine)

```
chat-stasher inbox-init <name> --locator <locator>
```

The locator names where objects live and never carries a credential. Two
backends are wired:

| Locator | Backend |
|---|---|
| `fs:///absolute/dir` | a directory (a synced folder, a mounted share) |
| `s3://<bucket>/<prefix>` | an S3-compatible bucket, for example Cloudflare R2 |

`inbox-init` contacts no backend. It saves the locator together with a fresh
age identity and prints the public recipient. Repeating the same declaration
keeps the recipient.

For `s3://` the prefix is mandatory and must be a path dedicated to this inbox
(`s3://my-bucket/inbox/laptop`). A bucket root, an empty or `..` segment, a
query string or embedded user info is refused, so one inbox can never list or
delete another's objects. Use one prefix per inbox and give no other tool write
access to it.

## Credentials for an s3 inbox

The endpoint and keys come from the environment, on every machine that touches
the bucket, and are never written to the declaration, a send key, a log or a
status line:

| Variable | Meaning |
|---|---|
| `CHAT_STASHER_INBOX_S3_ENDPOINT` | `https://<account>.r2.cloudflarestorage.com` or another S3 endpoint |
| `CHAT_STASHER_INBOX_S3_ACCESS_KEY_ID` | access key id |
| `CHAT_STASHER_INBOX_S3_SECRET_ACCESS_KEY` | secret access key |

The endpoint must be `https://`; plain `http://` is accepted only for a
loopback host (a local test server). Region is fixed to `auto`, which is what R2
expects, and no ambient AWS configuration or instance metadata is read.

Scope the keys to the prefix. The puller needs list, read and delete on it; a
producer needs write only. Give each side its own key pair.

If a variable is missing or empty, the command stops before any network call:
`send` exits 1, `inbox-pull` exits 3, and `status` and `doctor` show the
waiting count as unknown. A missing credential is never reported as an empty
inbox.

## Send (producer)

```
CHAT_STASHER_SEND_KEY=... chat-stasher send --path transcript.jsonl --session <native-id>
```

Issue the key on the archive machine with `chat-stasher send-key <name> --for
<platform> --credential-file <file>`. `send` reads the locator from the key. For
an `fs://` inbox the host's file permissions govern the write; for an `s3://`
inbox export the three variables above. An upload that cannot finish exits 3 and
leaves no resume receipt, so a retry sends again; an identical file already
uploaded is skipped (`resumed=1`).

## Pull (archive machine)

```
chat-stasher inbox-pull <name> --stage <dir> [--machine <name>]
```

The listing reads every page of the bucket, so an inbox with thousands of
objects is counted completely; a listing that fails part-way is an error (exit
3, "waiting count unknown"), never a shorter list. Each object is fetched,
verified against the issued policies, decrypted, sealed into the stage and then
checked against every declared destination. Only a proven object is deleted
from the inbox. Exit codes: 0 everything pulled and retired, 1 some object
refused (it stays), 3 something could not be read, so no "nothing waiting"
conclusion holds.

Retain `<name>.rates.sqlite3` next to the declaration; it holds the hourly
quota accounting.

## Status and doctor

`status` and `doctor` list each declared inbox with the number waiting, the age
of the oldest object, the last successful pull and the last pull's outcome. An
`s3://` inbox is listed live (bounded to 20 seconds) and labelled `backend=s3`;
`status --json` carries `"backend": "s3"` for it. If the bucket cannot be
reached or the credentials are not in the environment the state is `unknown`,
and the run ends incomplete (exit 3) instead of printing a zero.

## Testing

CI runs the S3 path against an in-process mock on loopback with synthetic keys.
The real-bucket round trip is opt-in:

```
CHAT_STASHER_R2_TEST_LIVE=1 \
CHAT_STASHER_R2_TEST_BUCKET=chat-stasher-inbox-test \
CHAT_STASHER_R2_TEST_ENDPOINT=... \
CHAT_STASHER_R2_TEST_ACCESS_KEY_ID=... \
CHAT_STASHER_R2_TEST_SECRET_ACCESS_KEY=... \
cargo test -p chat-stasher --test remote_inbox_r2_test
```

It refuses any bucket other than the dedicated test bucket and writes and
deletes only under a unique prefix it creates.
