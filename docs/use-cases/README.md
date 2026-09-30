# Use cases

<!-- RELEASE GATE: both pages in this folder describe behaviour merged after 0.5.0-rc.2 (cumulative `search` and `read`, the archive-only activity-index rebuild, and the per-format full-text index). Ship this folder with the release that carries it, or drop the pages it does not. -->

These pages start from a situation, not from a command. Each one is a task end to end: what it needs, the commands in order, and what their output actually proves.

Every transcript in them comes from a synthetic archive: the machines, session ids, dates and conversation lines are invented, and each page says how to rebuild that archive so you can re-run it yourself.

## What do you want to do?

| Your situation | Page | What it needs |
|---|---|---|
| A machine is gone (sold, wiped, dead) and you want the conversations it archived | [Recover a lost machine's conversations](lost-machine.md) | The destination, its key file, and any computer you can run from. The machine that wrote the archive is not needed. |
| You have a working archive and want a reusable procedure out of it | [Turn old conversations into a skill](skill-from-history.md) | A readable archive, its key file, and an agent to hand the exported files to. |

The first page ends where the second one starts: its output is the input of the other.

## If nothing here is your situation

- **You have not made an archive yet.** [start.md](../start.md) goes from nothing to a working one.
- **Something looks wrong already.** [troubleshooting.md](../troubleshooting.md) starts from what you see on screen.
- **You want one command's flags or exit codes.** [cli.md](../cli.md) is every command, and `chat-stasher <command> --help` is the same list on your machine.
- **You want to understand the archive before you rely on it.** [how-it-works.md](../how-it-works.md) covers the pipeline, the two reading tiers, and why an unknown is never reported as a zero.
