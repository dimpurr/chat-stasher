# Manual VoiceOver walk-through

The automated suites assert the keyboard package structurally — a skip link per
page, the access keys, the labelled form — but only a screen reader can say what
is *announced*. This is the one-time manual pass for that, and nobody claims it
was run unless a dated note below says so. It is written for macOS VoiceOver
because that is the screen reader the machine running the dashboard usually has;
the same walk on NVDA tells you the same things about the pages, with different
keys.

Start a dashboard the usual way (`chat-stasher ui`), turn VoiceOver on with
`Cmd+F5`, and walk the list, the reader, the session page and the search page.

## What to check, page by page

1. **The first stop is the skip link.** With a fresh page open, press `Tab`
   once. VoiceOver must announce "Skip to content, link" — not the pages nav
   and not a table row. Pressing it must land on `<main>`, which announces as
   the main landmark.
2. **The pages nav is a nav.** Open the rotor with `VO+U` and the landmarks
   list: *pages* (the three links overview · sessions · search) and *main* must
   both be there, plus *message pages* on a paginated list or conversation.
3. **One `h1` per page.** Walk headings with `VO+Cmd+H`: the first heading is
   the page's subject, the rest are `h2` sections below it.
4. **The list is a table with a caption** (`/sessions`): `VO+Cmd+X` walks by
   table, and the health/label cells read as words — "unknown", "no label
   recorded" — never as a zero that was not measured.
5. **The reader's messages are articles** (`/reader`): each bubble announces
   its role and time from the header, the collapsible blocks (thinking, tool
   calls) are inside native disclosures that VoiceOver reads as expandable,
   and the message anchors (`#m<N>`) are reachable from a search hit's link.
6. **The form says what it is** (`/search`): the input announces "Search
   sessions", the page opens with the focus already in the input (native
   `autofocus`), and the submit button is a button, not a script.
7. **The focus ring is visible in both colour schemes**: tab through a page in
   light and dark mode; every focused link shows the two-pixel outline. A
   clicked link must *not* grow one — the ring belongs to keyboard focus.
8. **The footer help line reads out**: the keyboard paragraph names every key
   (1, 2, f, `[`, `]`, r) and states the first Tab stop.

Known caveat, not a defect to file: VoiceOver's own modifier (`VO`, `Ctrl+Option`)
is also the modifier several macOS browsers assign to access keys, so with
VoiceOver running a key like `VO+]` may be eaten by the reader before the page
sees it. The walk does not depend on the keys — Tab order is the baseline and
the access keys are the accelerand; on a keyboard-only session without
VoiceOver, Chrome answers `Ctrl+Option+1` and Firefox answers
`Alt+Shift+1`.

## Record of runs

| Date | VoiceOver / macOS | Pages walked | Result | Notes |
|---|---|---|---|---|
| — | — | — | not yet run | a date and a verdict land here only after a human walks it |
