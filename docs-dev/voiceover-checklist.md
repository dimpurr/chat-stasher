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
2. **The named navs are distinct.** Open the rotor with `VO+U` and the
   landmarks list: *pages* (overview · sessions · search · Extensions) and
   *main* must both be there. On `/sessions`, also check *platform groups* and,
   when paginated, *session pages*; on a paginated `/reader`, check *message
   pages*. On `/search`, the search form is a search landmark.
3. **One `h1` per page.** Walk headings with `VO+Cmd+H`: the first heading is
   the page's subject, the rest are `h2` sections below it.
4. **The list is a table** (`/sessions`; it currently has no caption):
   `VO+Cmd+X` walks by table, and the time/label cells read as words —
   "unknown", "no label recorded" — never as a zero that was not measured.
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

Known caveat, not a defect to file: VoiceOver's own modifier (`VO`,
`Ctrl+Option`) overlaps the access-key modifier macOS browsers use. Chrome and
Safari use `Ctrl+Option+key`; Firefox uses `Ctrl+Option+key` or
`Ctrl+Alt+key`. With VoiceOver running, a chord such as `VO+]` may be handled
by the reader before the page sees it. The walk does not depend on access keys:
Tab order is the baseline and the access keys are an accelerator. See the
[browser and platform access-key combinations](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Global_attributes/accesskey#accessibility_concerns)
for current combinations and conflicts.

## Record of runs

| Date | VoiceOver / macOS | Pages walked | Result | Notes |
|---|---|---|---|---|
| — | — | — | not yet run | a date and a verdict land here only after a human walks it |
