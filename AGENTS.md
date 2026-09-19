# Agent entry point

The rules for working in this repository are in `CLAUDE.md`, and the commands
that must exit 0 before a change is proposed are in `CONTRIBUTING.md`. Read both
first. This file exists only because some agents do not open `CLAUDE.md` on their
own; it deliberately does not restate their contents, because two copies of a
rule drift apart and the copy nobody reads is the one that rots.

One rule matters enough to be stated in full in `CLAUDE.md` rather than only
summarized here: **every commit message, pull request title and body, issue,
code comment, and document here is written in English**, with the single
exception of `apps/extension/locales/zh_CN.yml`. Read that section of
`CLAUDE.md` — it is the single source for the language invariants, including
where the `commit-msg` hook and the CI checker live and how to enable the hook.
This file deliberately does not repeat the rule, because two copies of an
invariant drift apart.
