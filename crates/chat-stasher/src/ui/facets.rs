//! facets — the platform/agent grouping table and the session list's facet bar.
//!
//! 29-UI-DESIGN §2 (W137/UIA-3): every session's harness belongs to one of
//! three groups — **web platforms**, **coding agents**, or an honest
//! **ungrouped** bucket for ids this build does not classify. The grouping is
//! a static map on purpose: an id nobody classified stays visibly
//! unclassified until a release classifies it, never guessed into a group by
//! pattern or prefix. What the bar does for an unclassified id anyway is carry
//! it in the ungrouped link, so a platform the extension learns tomorrow is
//! filterable the moment it appears in the archive — without a release, just
//! not grouped under a name.
//!
//! A group is **not a new query parameter**. A group link carries the group's
//! ids as a plain multi-value `harness=` list — the same grammar the shared
//! selector reads as `--harness a,b` — so the bar can never express anything
//! `search` could not, [`crate::selector::SelectorArgs::resolve`] stays
//! untouched, and CLI parity (P7) is structural rather than asserted.
//!
//! The counts beside the groups are what clicking the link yields: every
//! *other* filter in force (the launch filter by construction, machine /
//! session / time from the URL) applies unchanged, and only the harness
//! dimension is replaced. Sessions no group can count — ids with no harness
//! prefix, or a harness containing the list separator, which the filter
//! grammar cannot name on the command line either — are not lost: they keep
//! the `All` count and get a note of their own, exactly the way the overview
//! matrix keeps its unlinked cells.

use std::collections::BTreeSet;

use crate::selector::{Resolved, Selector, Verdict};

use super::{percent_encode, Query, UiData};

/// The three honest groups a harness id can fall into (29-UI-DESIGN §2.1).
/// `Ungrouped` is a named bucket, never a silent catch-all: anything this
/// build does not classify lands there and is *labelled* as unclassified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformGroup {
    /// A browser-extension web chat platform: chatgpt.com, claude.ai, …
    WebPlatforms,
    /// A terminal/editor coding agent: claude-code, codex, opencode, …
    CodingAgents,
    /// An id this build does not classify. A new platform's first sessions
    /// land here, and stay filterable on their own id meanwhile.
    Ungrouped,
}

impl PlatformGroup {
    /// The word the page labels the group with, verbatim in the facet bar and
    /// the overview matrix header.
    pub fn label(self) -> &'static str {
        match self {
            PlatformGroup::WebPlatforms => "Web platforms",
            PlatformGroup::CodingAgents => "Coding agents",
            PlatformGroup::Ungrouped => "ungrouped",
        }
    }

    /// The word `/api/sessions` reports in each row's `platform_group`.
    pub fn wire(self) -> &'static str {
        match self {
            PlatformGroup::WebPlatforms => "web-platforms",
            PlatformGroup::CodingAgents => "coding-agents",
            PlatformGroup::Ungrouped => "ungrouped",
        }
    }

    /// The stylesheet class that colours the overview matrix's column headers
    /// by group (29-UI-DESIGN §3.1). Colour is never the only channel — the
    /// group's name sits in the header row right above — and every class draws
    /// on a variable the stylesheet already owns, per §8.1's "no new colours".
    pub fn css(self) -> &'static str {
        match self {
            PlatformGroup::WebPlatforms => "g-web",
            PlatformGroup::CodingAgents => "g-agents",
            PlatformGroup::Ungrouped => "g-ungrouped",
        }
    }
}

/// The separator the shared selector splits a `--harness`/`harness=` value
/// list on. One constant because the facet bar must agree with it exactly: a
/// group link *is* such a list, so an id containing this character cannot be
/// named by any harness filter — CLI or URL — and the bar says so rather than
/// linking something the grammar would splice into ids that do not exist.
const HARNESS_SEPARATOR: char = ',';

/// The web-chat platform ids the browser extension archives — the
/// `ALL_PLATFORMS` table of `apps/extension/lib/contract.ts`, the same
/// vocabulary activity.rs reads times for (`WEB_HARNESSES`). A static list
/// rather than one derived at render time, because which id belongs to which
/// group is a claim about the *kind of source*, decided per release, while
/// the ids one archive happens to hold are a property of that archive.
/// Refreshing this list is a deliberate change; a brand-new platform id does
/// **not** wait for one — it lands in `ungrouped` until this list learns it.
///
/// `grok` is here even though the CLI harness registry also has a `grok`
/// (models.rs): the registry id and the extension platform id are the same
/// string, both archive as `grok.<machine>.…`, and 29-UI-DESIGN §2.1 puts the
/// id under Web platforms. The archived id cannot tell the two apart; the
/// design chose the web reading for it, and this map follows.
const WEB_PLATFORM_HARNESSES: &[&str] = &[
    "chatgpt",
    "claude",
    "deepseek",
    "gemini",
    "grok",
    "kimi",
    "perplexity",
];

/// The coding-agent harness ids — one per harness in
/// `data/harness-registry-v1.json`, minus `grok` (that id belongs to the web
/// platform group above). A registry harness that appears without a home here
/// fails the map's test, so a new registry entry must be classified the same
/// release it is added — it may not silently fall into `ungrouped`.
const CODING_AGENT_HARNESSES: &[&str] = &[
    "aider",
    "claude-code",
    "codex",
    "continue",
    "crush",
    "cursor",
    "gemini-cli",
    "github-copilot-cli",
    "kimi-code",
    "opencode",
    "zed",
];

/// Classify a harness id. Web platforms are matched **first**: `grok` lives
/// in both lists' worlds and its group is the web one by design. Everything
/// not in either list — including platforms the extension learns after this
/// build — is `Ungrouped`, honestly.
pub fn group_of(harness: &str) -> PlatformGroup {
    if WEB_PLATFORM_HARNESSES.contains(&harness) {
        PlatformGroup::WebPlatforms
    } else if CODING_AGENT_HARNESSES.contains(&harness) {
        PlatformGroup::CodingAgents
    } else {
        PlatformGroup::Ungrouped
    }
}

/// Whether one harness id survives being written into a `harness=` value list
/// and read back as itself. The grammar splits on the separator, so an id
/// containing one would come back as shorter ids that match nothing — the CLI
/// cannot spell such an id either, and both surfaces (`search`'s bar and the
/// overview matrix's cells) say so instead of linking it.
pub fn is_expressible_as_filter_value(harness: &str) -> bool {
    !harness.contains(HARNESS_SEPARATOR)
}

/// The harness ids a group's link carries: the static sets for the two named
/// groups, and — for `ungrouped`, whose vocabulary cannot be enumerated by
/// definition — the unclassified ids actually present in the launch view,
/// minus the ones the filter grammar cannot name at all. A BTreeSet iterates
/// sorted, so a link is byte-stable across renders and parses back to the same
/// set the shared selector holds.
fn group_values(group: PlatformGroup, data: &UiData) -> Vec<String> {
    let values: BTreeSet<String> = match group {
        PlatformGroup::WebPlatforms => WEB_PLATFORM_HARNESSES
            .iter()
            .map(|h| (*h).to_string())
            .collect(),
        PlatformGroup::CodingAgents => CODING_AGENT_HARNESSES
            .iter()
            .map(|h| (*h).to_string())
            .collect(),
        // The ungrouped vocabulary is what the archive shows, not what this
        // build could dream up: an id present in the view is filterable, and
        // an id hypothesised here might not have been a harness at all.
        PlatformGroup::Ungrouped => data
            .sessions
            .iter()
            .filter_map(|s| s.harness.as_deref())
            .filter(|h| {
                group_of(h) == PlatformGroup::Ungrouped && is_expressible_as_filter_value(h)
            })
            .map(|h| h.to_string())
            .collect(),
    };
    values.into_iter().collect()
}

/// The page's selector with the harness dimension removed: everything else in
/// force (launch filter, machine, session, time) stays, so a group link's
/// count is measured under exactly the constraints the click keeps.
fn base_selector(resolved: &Resolved) -> Selector {
    let mut base = resolved.selector.clone();
    base.harnesses = None;
    base
}

/// The counts the bar shows, bucketed in one pass over the rows the base
/// selector matches. Every bucket is a measurement, so the page can name each
/// one and their sum is the `All` count by construction.
struct Buckets {
    all: usize,
    web: usize,
    agents: usize,
    ungrouped: usize,
    no_prefix: usize,
    /// Sessions whose harness contains the list separator: real sources,
    /// countable, and unnameable by any harness filter.
    unexpressible: usize,
}

fn buckets(data: &UiData, base: &Selector) -> Buckets {
    let mut out = Buckets {
        all: 0,
        web: 0,
        agents: 0,
        ungrouped: 0,
        no_prefix: 0,
        unexpressible: 0,
    };
    for s in &data.sessions {
        if base.select(&s.meta()) != Verdict::Selected {
            continue;
        }
        out.all += 1;
        match s.harness.as_deref() {
            None => out.no_prefix += 1,
            Some(h) if !is_expressible_as_filter_value(h) => out.unexpressible += 1,
            Some(h) => match group_of(h) {
                PlatformGroup::WebPlatforms => out.web += 1,
                PlatformGroup::CodingAgents => out.agents += 1,
                PlatformGroup::Ungrouped => out.ungrouped += 1,
            },
        }
    }
    out
}

/// The URL a facet link points at: this page's query with the harness
/// dimension replaced by `harness_values` (or removed for `None`, the All
/// item), everything else carried over. Only the vocabulary the list routes
/// read survives, each key's **first** value — the same one
/// `selector_from_query` (and `page_href` for the paging links) reads, so a
/// link cannot smuggle in a second meaning for a key. `offset` is dropped
/// rather than carried: a change of set must start at that set's page 1, and
/// landing mid-window of a different list would present a window, not the
/// state the link promises.
fn facet_href(params: &Query, token: &str, harness_values: Option<&[String]>) -> String {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in params {
        if !matches!(
            key.as_str(),
            "session" | "machine" | "day" | "since" | "until" | "sort" | "limit"
        ) || !seen.insert(key.as_str())
        {
            continue;
        }
        parts.push(format!("{}={}", percent_encode(key), percent_encode(value)));
    }
    if let Some(values) = harness_values {
        // The separator sits *between* the percent-encoded values; the values
        // themselves are separator-free by construction (see
        // `is_expressible_as_filter_value`), so the list splits back into
        // exactly these ids on the server.
        parts.push(format!(
            "harness={}",
            values
                .iter()
                .map(|v| percent_encode(v))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    parts.push(format!("token={}", percent_encode(token)));
    format!("/sessions?{}", parts.join("&"))
}

/// Why an empty ungrouped item is not a link: the link would name no id at
/// all, and a filter that names nothing is not "everything unclassified" but
/// the nothing-matching list. Same shape of honesty as the matrix's unlinked
/// no-harness cells, whose title explains themselves the same way.
const UNGROUPED_ABSENT_TITLE: &str =
    "no source in this view is unclassified, so the ungrouped filter would name no id at all";

/// One bar item. `values: None` is the All item — no harness constraint at
/// all; `values: Some(list)` is a group link whose list it carries.
struct FacetItem {
    group: Option<PlatformGroup>,
    count: usize,
    values: Option<Vec<String>>,
}

/// The facet bar of `/sessions` (29-UI-DESIGN §2.3): `All · Web platforms ·
/// Coding agents · ungrouped`, each with the count the click yields, each
/// non-current item a plain GET link that inherits every other facet in
/// force, the current item marked `aria-current` — and only the item whose
/// value set is **exactly** the page's harness constraint: a strict subset
/// (a matrix cell drill-down named one agent, say) marks nothing, because
/// clicking any group would change the set the page is showing, and the
/// Filter note above keeps naming that exact truth already.
pub(super) fn facet_bar(resolved: &Resolved, params: &Query, token: &str, data: &UiData) -> String {
    let base = base_selector(resolved);
    let b = buckets(data, &base);
    let groups = [
        (PlatformGroup::WebPlatforms, b.web),
        (PlatformGroup::CodingAgents, b.agents),
        (PlatformGroup::Ungrouped, b.ungrouped),
    ];
    let items: Vec<FacetItem> = std::iter::once(FacetItem {
        group: None,
        count: b.all,
        values: None,
    })
    .chain(groups.into_iter().map(|(group, count)| FacetItem {
        group: Some(group),
        count,
        values: Some(group_values(group, data)),
    }))
    .collect();

    let mut parts: Vec<String> = Vec::new();
    for item in &items {
        // Set equality, never subset: All is current exactly when the page
        // carries no harness constraint, a group exactly when the page's
        // harness list is the group's own list.
        let current = match &item.values {
            None => resolved.selector.harnesses.is_none(),
            Some(values) => resolved.selector.harnesses == Some(values.iter().cloned().collect()),
        };
        let label = match item.group {
            None => "All",
            Some(group) => group.label(),
        };
        let text = format!("{} {}", label, item.count);
        let cell = if current {
            // The page is in this item's state; printed, not linked, the same
            // way the paging nav prints the current page.
            format!("<b aria-current=\"true\">{text}</b>")
        } else if item.values.as_ref().is_some_and(|v| v.is_empty()) {
            // An empty set has nothing to filter on. No link, and the reason
            // in the title, mirroring the matrix's unlinked no-harness cells.
            format!("<span title=\"{UNGROUPED_ABSENT_TITLE}\">{text}</span>")
        } else {
            let values = item.values.as_deref();
            format!(
                "<a href=\"{}\">{text}</a>",
                facet_href(params, token, values)
            )
        };
        parts.push(cell);
    }

    let mut out = format!(
        "<nav class=sub aria-label=\"platform groups\">Platform groups: {}</nav>\n",
        parts.join(" · ")
    );
    out.push_str(
        "<p class=sub>Each group link carries every other filter in force unchanged and \
         replaces the harness filter with its group's sources — the same <code>--harness</code> \
         list <code>search</code> reads — so it yields this list over a different harness set. \
         The number beside a group is the number of session(s) that link's page reports \
         matched. Sources this build does not classify sit under <i>ungrouped</i> and are \
         still filterable on their own.</p>\n",
    );
    // Two honest gaps, each named only when it exists in this view, because a
    // note about rows that are not there would itself be a false claim.
    if b.no_prefix > 0 {
        out.push_str(&format!(
            "<p class=sub>{} session(s) in view carry no group: their archived id has no \
             harness prefix, so no <code>--harness</code> filter can select them. They are \
             counted under <b>All</b> only — a group link lists them under \"could not be \
             placed\", not as absent.</p>\n",
            b.no_prefix
        ));
    }
    if b.unexpressible > 0 {
        out.push_str(&format!(
            "<p class=sub>{} session(s) in view carry a harness id no <code>--harness</code> \
             filter can name: the id contains the list separator, which the filter grammar \
             splits into separate ids (on the command line too). They are counted under \
             <b>All</b> only.</p>\n",
            b.unexpressible
        ));
    }
    out
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;
    use crate::selector::SelectorArgs;

    fn req(target: &str, data: &crate::ui::UiData) -> String {
        let (path, params) = crate::ui::split_target(target);
        crate::ui::handle(path, &params, "t", data, &crate::ui::NoContent)
            .unwrap_or_else(|| panic!("`{target}` must be a known route"))
            .body
    }

    /// The bar block of a rendered list page: everything inside the nav that
    /// carries the platform-groups label, where the links and their counts
    /// live.
    fn bar_of(page: &str) -> &str {
        let start = page
            .find("<nav class=sub aria-label=\"platform groups\">")
            .unwrap_or_else(|| panic!("no platform-group nav in {page}"));
        let end = page[start..]
            .find("</nav>\n")
            .map(|o| start + o)
            .unwrap_or_else(|| panic!("the platform-group nav never closes in {page}"));
        &page[start..end]
    }

    /// Every `(href, label)` link cell of the bar, label verbatim (it carries
    /// the advertised count as its last word).
    fn links_of(bar: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = bar;
        while let Some(at) = rest.find("<a href=\"") {
            let after = &rest[at + 9..];
            let (href, rest_cell) = after
                .split_once("\">")
                .unwrap_or_else(|| panic!("a cell without its label text in {bar}"));
            let label = rest_cell
                .split_once("</a>")
                .unwrap_or_else(|| panic!("a cell that never closes: {bar}"))
                .0;
            out.push((href.to_string(), label.to_string()));
            rest = &after[href.len()..];
        }
        out
    }

    /// The grouping map is pinned against both vocabularies it mirrors — the
    /// extension's platform ids and the registry's harness ids — so a new id
    /// in either source cannot silently fall into `ungrouped`: it fails here
    /// until the map learns it. The one deliberate resident of `ungrouped` is
    /// an id this build has never heard of at all.
    #[test]
    fn the_grouping_map_covers_every_registry_and_platform_id() {
        for id in WEB_PLATFORM_HARNESSES {
            assert_eq!(
                group_of(id),
                PlatformGroup::WebPlatforms,
                "`{id}` is an extension platform id, so its group is Web platforms"
            );
        }
        // The near-collisions first: one prefix is not its neighbour's group.
        assert_eq!(group_of("kimi"), PlatformGroup::WebPlatforms);
        assert_eq!(group_of("kimi-code"), PlatformGroup::CodingAgents);
        assert_eq!(group_of("claude"), PlatformGroup::WebPlatforms);
        assert_eq!(group_of("claude-code"), PlatformGroup::CodingAgents);
        assert_eq!(
            group_of("grok"),
            PlatformGroup::WebPlatforms,
            "the design puts the shared `grok` id under Web platforms (29-UI-DESIGN §2.1)"
        );
        let registry = crate::scanner::load_registry_from_repo()
            .expect("the embedded harness registry must parse for this test");
        for h in &registry.harnesses {
            match group_of(&h.id) {
                PlatformGroup::CodingAgents => {}
                PlatformGroup::WebPlatforms => {
                    assert_eq!(
                        h.id, "grok",
                        "only `grok` is a web platform among registry harnesses"
                    );
                }
                PlatformGroup::Ungrouped => panic!(
                    "registry harness `{}` is unclassified in the grouping map: classify it \
                     in ui/facets.rs, or it will read as an unknown platform",
                    h.id
                ),
            }
        }
        // And an id this build has never heard of — the new-platform case the
        // TODO's fixture asks for — is ungrouped, not guessed.
        assert_eq!(group_of("omega-web"), PlatformGroup::Ungrouped);
        assert_eq!(
            group_of("brand-new-platform-from-the-extension"),
            PlatformGroup::Ungrouped
        );
    }

    /// `fixture::groups_data`'s hand-laid rows, as the known answers every
    /// bar-count assertion below is derived from — never by running the code
    /// under test: two agents, two web platforms, one unclassified id, one
    /// no-prefix id, one id the filter grammar cannot name.
    #[test]
    fn groups_data_is_laid_out_as_documented() {
        let d = fixture::groups_data();
        let laid_out = [
            (0, "claude-code", PlatformGroup::CodingAgents),
            (1, "deepseek", PlatformGroup::WebPlatforms),
            (2, "grok", PlatformGroup::WebPlatforms),
            (3, "omega-web", PlatformGroup::Ungrouped),
            (4, "claude-code", PlatformGroup::CodingAgents),
        ];
        for (index, harness, group) in laid_out {
            let s = d
                .sessions
                .iter()
                .find(|s| s.index == index)
                .unwrap_or_else(|| panic!("row {index} missing"));
            assert_eq!(s.harness.as_deref(), Some(harness), "row {index}");
            assert_eq!(group_of(s.harness.as_deref().unwrap()), group);
        }
        assert_eq!(d.sessions[5].harness, None, "row 5 has no prefix");
        assert_eq!(
            d.sessions[6].harness.as_deref(),
            Some("we,ird"),
            "row 6 carries the separator id"
        );
    }

    /// The counts the bar names over the unfiltered view, derived by hand:
    /// All 7, Web platforms 2 (deepseek, grok), Coding agents 2, ungrouped 1
    /// (omega-web — `we,ird` cannot be linked and is excluded), and the two
    /// gap notes with their counts.
    #[test]
    fn bar_counts_are_the_hand_laid_bucket_totals() {
        let d = fixture::groups_data();
        let page = req("/sessions", &d);
        let bar = bar_of(&page);
        assert!(
            bar.contains("<b aria-current=\"true\">All 7</b>"),
            "bar: {bar}"
        );
        assert!(bar.contains(">Web platforms 2</a>"), "bar: {bar}");
        assert!(bar.contains(">Coding agents 2</a>"), "bar: {bar}");
        assert!(bar.contains(">ungrouped 1</a>"), "bar: {bar}");
        assert!(
            page.contains(
                "1 session(s) in view carry no group: their archived id has no harness prefix"
            ),
            "{page}"
        );
        assert!(
            page.contains(
                "1 session(s) in view carry a harness id no <code>--harness</code> filter can \
                 name"
            ),
            "{page}"
        );
    }

    /// The sum invariant: every bucket count adds up to All, and the notes
    /// appear only for buckets that exist. On the plain fixture — no
    /// unclassified ids at all — the ungrouped item is unlinked text with its
    /// reason in the title, and neither note renders.
    #[test]
    fn buckets_sum_to_all_and_notes_match_their_gaps() {
        let d = fixture::groups_data();
        let resolved = SelectorArgs::default().resolve().unwrap();
        let b = buckets(&d, &base_selector(&resolved));
        assert_eq!(
            b.all,
            b.web + b.agents + b.ungrouped + b.no_prefix + b.unexpressible,
            "every matched row lands in exactly one bucket"
        );
        assert_eq!(
            (
                b.all,
                b.web,
                b.agents,
                b.ungrouped,
                b.no_prefix,
                b.unexpressible
            ),
            (7, 2, 2, 1, 1, 1)
        );

        let plain = fixture::data();
        let page = req("/sessions", &plain);
        let bar = bar_of(&page);
        assert!(
            bar.contains("<b aria-current=\"true\">All 4</b>"),
            "bar: {bar}"
        );
        assert!(bar.contains(">Web platforms 1</a>"), "bar: {bar}");
        assert!(bar.contains(">Coding agents 2</a>"), "bar: {bar}");
        let ungrouped_cell = bar
            .split("·")
            .find(|cell| cell.contains("ungrouped 0"))
            .unwrap_or_else(|| panic!("the ungrouped item with its count: {bar}"));
        assert!(
            ungrouped_cell.contains(&format!("title=\"{UNGROUPED_ABSENT_TITLE}\"")),
            "an empty ungrouped set is text with its reason, never a link: {bar}"
        );
        assert!(
            !ungrouped_cell.contains("href="),
            "an empty value set cannot be a filter: {bar}"
        );
        // The plain fixture holds one no-prefix session (`.hidden-session`),
        // so that note states its one row — and no separator-carrying id
        // exists there, so that note must not render.
        assert!(
            page.contains(
                "1 session(s) in view carry no group: their archived id has no harness prefix"
            ),
            "{page}"
        );
        assert!(
            !page.contains("harness id no <code>--harness</code> filter can name"),
            "{page}"
        );
    }

    /// **The pin.** Every facet link parses back through the page's own
    /// [`crate::ui::selector_from_query`], and the selector it yields equals
    /// the one the equivalent command-line flags produce — the `--harness`
    /// list read as the value list it is. And the count a link advertises is
    /// the matched count its own page reports, so the bar's number can never
    /// drift from the set the click shows. Checked under three different
    /// filter states, including one that narrows the time window, because a
    /// count that ignored the time facet would pass the unfiltered run alone.
    #[test]
    fn every_facet_link_parses_back_to_the_cli_equivalent_selector() {
        let d = fixture::groups_data();
        for base_query in [
            "",
            "machine=m-2",
            "machine=m-2&since=2024-01-01&until=2100-01-01",
        ] {
            let target = if base_query.is_empty() {
                "/sessions".to_string()
            } else {
                format!("/sessions?{base_query}")
            };
            let page = req(&target, &d);
            let bar = bar_of(&page);
            let links = links_of(bar);
            assert!(!links.is_empty(), "the bar must carry links: {bar}");
            for (href, label) in &links {
                let (_, params) = crate::ui::split_target(href);
                let from_href = crate::ui::selector_from_query(&params)
                    .unwrap_or_else(|e| panic!("`{href}` does not parse back: {e}"));
                // The CLI-equivalent SelectorArgs, built the way clap's
                // value_delimiter builds it: one list, split on the separator.
                let harness_list = params.iter().find(|(k, _)| k == "harness").map(|(_, v)| {
                    v.split(HARNESS_SEPARATOR)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                });
                let cli = SelectorArgs {
                    session: params
                        .iter()
                        .find(|(k, _)| k == "session")
                        .map(|(_, v)| v.clone()),
                    machine: params
                        .iter()
                        .find(|(k, _)| k == "machine")
                        .map(|(_, v)| v.clone()),
                    harness: harness_list,
                    day: params
                        .iter()
                        .find(|(k, _)| k == "day")
                        .map(|(_, v)| v.clone()),
                    since: params
                        .iter()
                        .find(|(k, _)| k == "since")
                        .map(|(_, v)| v.clone()),
                    until: params
                        .iter()
                        .find(|(k, _)| k == "until")
                        .map(|(_, v)| v.clone()),
                    ..Default::default()
                }
                .resolve()
                .unwrap_or_else(|e| panic!("the CLI-equivalent flags of `{href}` refuse: {e}"));
                assert_eq!(
                    from_href.selector, cli.selector,
                    "`{href}` parses back differently than its CLI flags"
                );
                // The link must not have smuggled a paging position: a
                // changed set starts at its own page 1.
                assert!(
                    !params.iter().any(|(k, _)| k == "offset"),
                    "`{href}` carries an offset"
                );

                // What the link's page reports matched == the count the bar
                // advertised beside it.
                let query = href
                    .split_once('?')
                    .map(|(_, q)| q.to_string())
                    .unwrap_or_default();
                let advertised: usize = label
                    .rsplit_once(' ')
                    .and_then(|(_, n)| n.parse().ok())
                    .unwrap_or_else(|| panic!("no count in the `{label}` of {href}"));
                let body = req(&format!("/api/sessions?{query}"), &d);
                let v: serde_json::Value =
                    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
                let matched = v["matched"].as_u64().unwrap() as usize;
                assert_eq!(
                    matched, advertised,
                    "`{href}` matched {matched} but the bar advertised {advertised}"
                );
            }
        }
    }

    /// The current item is the exact state, and only the exact state: the
    /// full agents list marks Coding agents; one agent id among eleven marks
    /// nothing, because clicking the group would show a different set; no
    /// harness constraint marks All.
    #[test]
    fn the_current_group_is_marked_only_for_the_exact_set() {
        let d = fixture::groups_data();
        let agents_list = group_values(PlatformGroup::CodingAgents, &d).join(",");
        let page = req(&format!("/sessions?harness={agents_list}"), &d);
        let bar = bar_of(&page);
        assert!(
            bar.contains("<b aria-current=\"true\">Coding agents 2</b>"),
            "the full agents list is the group's own set, so it is current: {bar}"
        );
        let page = req("/sessions?harness=claude-code", &d);
        let bar = bar_of(&page);
        assert!(
            !bar.contains("aria-current"),
            "one agent id is a subset of the group, not the group itself: {bar}"
        );
        let page = req("/sessions", &d);
        let bar = bar_of(&page);
        assert!(
            bar.contains("<b aria-current=\"true\">All 7</b>"),
            "no harness constraint is the All state: {bar}"
        );
    }

    /// A launch filter cannot be escaped by a facet link: the bar counts (and
    /// the links therefore yield) rows inside the launch filter only, because
    /// the inventory they are computed from already carries it.
    #[test]
    fn facet_links_stay_inside_the_launch_filter() {
        let report = fixture::groups_report();
        let launch = Selector::default().machine("m-2");
        let d = crate::ui::UiData::from_report(&report, "dest-under-test", launch, fixture::NOW);
        let page = req("/sessions", &d);
        let bar = bar_of(&page);
        // On m-2 the rows are grok (web), omega-web (ungrouped), claude-code
        // (agents), the no-prefix id and the separator id — five in all.
        assert!(
            bar.contains("<b aria-current=\"true\">All 5</b>"),
            "bar: {bar}"
        );
        assert!(bar.contains(">Web platforms 1</a>"), "bar: {bar}");
        assert!(bar.contains(">Coding agents 1</a>"), "bar: {bar}");
        assert!(bar.contains(">ungrouped 1</a>"), "bar: {bar}");
    }

    /// The ungrouped link carries the actual unclassified ids present in the
    /// view — the TODO's "a new platform automatically lands in ungrouped" —
    /// and never an id the grammar would splice into others.
    #[test]
    fn the_ungrouped_link_carries_the_real_unclassified_ids() {
        let d = fixture::groups_data();
        let page = req("/sessions", &d);
        let bar = bar_of(&page);
        let naming_omega: Vec<(String, String)> = links_of(bar)
            .into_iter()
            .filter(|(href, _)| href.contains("omega-web"))
            .collect();
        assert_eq!(
            naming_omega.len(),
            1,
            "exactly the ungrouped link names omega-web: {bar}"
        );
        let href = &naming_omega[0].0;
        assert!(
            href.contains("harness=omega-web")
                && !href.contains("we%2Cird")
                && !href.contains("we,ird"),
            "the separator-carrying id is never linked: {href}"
        );
    }
}
