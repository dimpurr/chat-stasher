//! reader — the `/reader` route and the rendering of one normalized message.
//!
//! The reader is the only other route that reaches the payload tier. It escapes
//! first and then emits a bounded subset of markup, links only to local targets,
//! and elides anything over the per-message budget with a measured byte count
//! and a link to the raw shards.

use super::html::{esc, fmt_bytes, fmt_unix, footer, head};
use super::{
    index_param, param, percent_encode, ContentSource, Query, Response, UiData, UiSession,
};

pub(super) fn reader_page(
    params: &Query,
    token: &str,
    data: &UiData,
    content: &dyn ContentSource,
) -> Response {
    let Some(row) = index_param(params, data) else {
        return Response::text(
            400,
            "Bad Request",
            "ui: `i` must name a row of this dashboard's session list. An index that \
             resolves to nothing is a usage error, not an empty session.\n",
        );
    };
    let (start, width) = match reader_window(params) {
        Ok(window) => window,
        Err(message) => return Response::text(400, "Bad Request", format!("ui: {message}\n")),
    };
    match content.fetch(&row.machine, &row.session_id) {
        Ok(c) => {
            let conversation =
                crate::normalize::normalize(row.harness.as_deref().unwrap_or(""), &c.body);
            Response::html(
                200,
                "OK",
                page_reader(row, token, data, &conversation, start, width),
            )
        }
        Err(e) => Response::text(
            502,
            "Bad Gateway",
            format!(
                "ui: could not read session {} on `{}`: {e}\n\
                 This is a failure to read, NOT an empty session.\n",
                row.short_id, row.machine
            ),
        ),
    }
}

pub(super) fn reader_window(params: &Query) -> Result<(usize, usize), String> {
    let start = match param(params, "m") {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| "`m` must be a non-negative message offset".to_string())?,
        None => 0,
    };
    let width = match param(params, "n") {
        Some(raw) => {
            let parsed = raw
                .parse::<usize>()
                .map_err(|_| "`n` must be a positive message window".to_string())?;
            if parsed == 0 {
                return Err("`n` must be a positive message window".to_string());
            }
            parsed.min(crate::normalize::MAX_WINDOW)
        }
        None => crate::normalize::DEFAULT_WINDOW,
    };
    Ok((start, width))
}

// --------------------------------------------------------------- reader page

fn page_reader(
    s: &UiSession,
    token: &str,
    data: &UiData,
    conversation: &crate::normalize::Conversation,
    start: usize,
    width: usize,
) -> String {
    let total = conversation.messages.len();
    let end = start.saturating_add(width).min(total);
    let mut out = head(&format!("chat-stasher · reader · {}", s.short_id));
    out.push_str(&format!(
        "<h1>Conversation <span class=mono>{sid}</span></h1>\n\
         <p class=sub><a href=\"/session?i={i}&token={t}\">← session metadata</a> · \
         <a href=\"/content?i={i}&token={t}\">show raw shards</a> · destination <b>{d}</b></p>\n",
        sid = esc(&s.short_id),
        i = s.index,
        t = percent_encode(token),
        d = esc(&data.destination_label),
    ));
    out.push_str(&reader_provenance(conversation));
    if conversation.branch_nodes > 0 || !conversation.canonical_follows_active {
        if conversation.canonical_follows_active {
            out.push_str(&format!(
                "<div class=note>Following the active branch. {} branch node(s) are not shown.</div>\n",
                conversation.branch_nodes
            ));
        } else {
            out.push_str(
                "<div class=warn>Branch provenance is unknown; the displayed messages are not claimed to be the complete active branch.</div>\n",
            );
        }
    }
    if conversation.message_total == 0 {
        // "No conversation content" (ADR-035) is a claim about the archive: the
        // session holds nothing to place in time. A body that does have lines,
        // none of which this reader could turn into a message, is the other
        // state and must be worded as itself — the coverage line below says how
        // many lines were involved and the raw link is where they are audited.
        if conversation.unrendered_lines > 0 || conversation.unrecognized_lines > 0 {
            out.push_str(
                "<div class=warn>Nothing in this session's body could be rendered as a message. \
                 This is not an empty conversation: the body has lines, and they are counted \
                 below.</div>\n",
            );
        } else {
            out.push_str("<div class=note>no conversation content</div>\n");
        }
    } else if start >= total {
        // An offset past the end is not a missing conversation and not a
        // usage error: the window is empty, which is a different claim from
        // both, so it is stated as itself and a way back is offered.
        out.push_str(&format!(
            "<div class=note>no messages in this window: this conversation has {} message(s) \
             and the window starts after the last one.</div>\n",
            total
        ));
        out.push_str(&reader_nav(s, token, start, width, total));
    } else {
        out.push_str(&format!(
            "<p class=sub>Showing messages {}–{} of {}.</p>\n",
            start.saturating_add(1),
            end,
            conversation.message_total
        ));
        let raw_href = format!("/content?i={}&token={}", s.index, percent_encode(token));
        for (index, message) in conversation.messages[start..end].iter().enumerate() {
            out.push_str(&render_message(start + index, message, &raw_href));
        }
        out.push_str(&reader_nav(s, token, start, width, total));
    }
    if conversation.unrendered_lines > 0 || conversation.unrecognized_lines > 0 {
        out.push_str(&format!(
            "<div class=warn>Reader coverage: {} line(s) were not rendered and {} line(s) were not valid JSON. \
             <a href=\"/content?i={i}&token={t}\">Open raw shards to audit them.</a></div>\n",
            conversation.unrendered_lines,
            conversation.unrecognized_lines,
            i = s.index,
            t = percent_encode(token),
        ));
    }
    out.push_str(&footer(data));
    out.push_str("</body></html>\n");
    out
}

fn reader_provenance(conversation: &crate::normalize::Conversation) -> String {
    let text = match &conversation.provenance {
        crate::normalize::Provenance::Known { source } => {
            format!(
                "<p class=sub>Provenance: archived {} records.</p>\n",
                esc(source)
            )
        }
        crate::normalize::Provenance::Unknown { why } => {
            format!("<div class=warn>Provenance unknown: {}</div>\n", esc(why))
        }
    };
    text
}

/// Window links, in the order the design's wireframe draws them: previous,
/// next, first, last. Every one is a plain `<a>` with the window in its query,
/// so paging works with no script and each page is a URL worth bookkeeping.
fn reader_nav(s: &UiSession, token: &str, start: usize, width: usize, total: usize) -> String {
    let width = width.max(1);
    let last_start = total.saturating_sub(1) / width * width;
    let mut links = Vec::new();
    let push = |label: &str, m: usize, links: &mut Vec<String>| {
        links.push(format!(
            "<a href=\"/reader?i={}&m={}&n={}&token={}\">{}</a>",
            s.index,
            m,
            width,
            percent_encode(token),
            label
        ));
    };
    if start > 0 {
        push(
            "← previous messages",
            start.saturating_sub(width),
            &mut links,
        );
    }
    if start.saturating_add(width) < total {
        push("next messages →", start.saturating_add(width), &mut links);
    }
    if start > 0 {
        push("first messages", 0, &mut links);
    }
    if last_start > start {
        push("last messages", last_start, &mut links);
    }
    if links.is_empty() {
        String::new()
    } else {
        format!(
            "<nav class=sub aria-label=\"message pages\">{}</nav>\n",
            links.join(" · ")
        )
    }
}

fn render_message(index: usize, message: &crate::normalize::Message, raw_href: &str) -> String {
    let time = match &message.time {
        crate::normalize::MessageTime::Known { unix, source } => format!(
            "<time>{}</time> · {}",
            esc(&fmt_unix(*unix)),
            time_source_label(source)
        ),
        crate::normalize::MessageTime::Unknown { why } => {
            format!("<span class=bad title=\"{}\">time unknown</span>", esc(why))
        }
    };
    // One budget for the whole message, not one per block: a turn made of many
    // medium parts is the same hazard as one huge part.
    let mut remaining = crate::normalize::MESSAGE_BUDGET;
    let blocks = message
        .blocks
        .iter()
        .map(|block| render_block(block, &mut remaining, raw_href))
        .collect::<String>();
    format!(
        "<article id=\"m{index}\" class=message><header><b>{}</b> · {} </header>{blocks}</article>\n",
        message.role.label(),
        time,
    )
}

/// Name where a message's timestamp came from, in the vocabulary
/// `activity::TimeSource` already uses for a session span. A time the reader
/// had to interpret is not the same claim as one the harness recorded.
fn time_source_label(source: &crate::activity::TimeSource) -> String {
    use crate::activity::TimeSource;
    match source {
        TimeSource::Exact => "exact".to_string(),
        TimeSource::Inferred { how } => format!("inferred ({})", esc(how)),
        TimeSource::Messages { exact: true } => "from messages, exact".to_string(),
        TimeSource::Messages { exact: false } => "from messages, interpreted".to_string(),
        TimeSource::ListUpdated => "conversation list updated".to_string(),
        TimeSource::NoConversationContent => "no conversation content".to_string(),
        TimeSource::PartialRange { how, .. } => format!("partial range ({})", esc(how)),
        TimeSource::Unknown { .. } => "unknown".to_string(),
    }
}

/// Render one block within the message's remaining budget. Anything that does
/// not fit is cut to its head and tail and the byte count in between is
/// printed with a link to the raw shards, so a large message costs a bounded
/// page without the hidden part being denied.
fn render_block(block: &crate::normalize::Block, remaining: &mut usize, raw_href: &str) -> String {
    match block {
        crate::normalize::Block::Text(text) => {
            if text.len() <= *remaining {
                *remaining -= text.len();
                render_markdown(text)
            } else {
                render_elided(text, remaining, raw_href)
            }
        }
        crate::normalize::Block::CodeBlock { language, code } => {
            let class = language
                .as_deref()
                .map(|lang| format!(" class=\"language-{}\"", esc(lang)))
                // reason: no language was recorded, so no class attribute is
                // written. The empty string is the absence of an attribute,
                // not a language named "".
                .unwrap_or_default();
            if code.len() <= *remaining {
                *remaining -= code.len();
                format!("<pre><code{class}>{}</code></pre>\n", esc(code))
            } else {
                render_elided(code, remaining, raw_href)
            }
        }
        crate::normalize::Block::Thinking(text) => format!(
            "<details class=thinking><summary>Thinking</summary>{}</details>\n",
            if text.len() <= *remaining {
                *remaining -= text.len();
                render_markdown(text)
            } else {
                render_elided(text, remaining, raw_href)
            }
        ),
        crate::normalize::Block::ToolCall {
            name,
            input_summary,
            output_bytes,
        } => {
            // The recorded input is shown as bytes rather than as Markdown:
            // it is a JSON argument list, and the reader does not claim to
            // know what a tool would have done with it.
            let body = if input_summary.len() <= *remaining {
                *remaining -= input_summary.len();
                format!("<pre>{}</pre>\n", esc(input_summary))
            } else {
                render_elided(input_summary, remaining, raw_href)
            };
            // The output size is written only when a record carried one: a
            // tool call whose result was not archived has no output size, and
            // "output 0 B" would be the reader inventing one.
            let output = match output_bytes {
                Some(bytes) => format!(" · output {}", esc(&fmt_bytes(*bytes as u64))),
                None => " · output not recorded".to_string(),
            };
            format!(
                "<details class=tool><summary>Tool call{}{output}</summary>{body}</details>\n",
                name.as_deref()
                    .map(|name| format!(": {}", esc(name)))
                    // reason: an unnamed tool call keeps its number and its
                    // body; the name is simply absent, and no placeholder
                    // name is substituted for it.
                    .unwrap_or_default(),
            )
        }
        crate::normalize::Block::AttachmentRef(attachment) => format!(
            "<p class=attachment>Attachment reference: <span class=mono>{}</span> · {} · {}</p>\n",
            esc(attachment.name.as_deref().unwrap_or("unnamed")),
            esc(attachment.media_type.as_deref().unwrap_or("type unknown")),
            attachment
                .bytes
                .map(|bytes| esc(&fmt_bytes(bytes)))
                .unwrap_or_else(|| "size unknown".to_string())
        ),
    }
}

/// The over-budget branch: keep the head and the tail, print how much was
/// dropped, and link to the bytes themselves. The note lives inside the
/// `<pre>` so the elision is visible exactly where the text stops, and the
/// count is a measurement rather than a placeholder for what is missing.
///
/// A text that does not fit is rendered as `<pre>` rather than as Markdown:
/// a truncated Markdown document can end mid-construct, and the point of this
/// path is that what is shown is the recorded bytes and nothing derived
/// from them.
fn render_elided(text: &str, remaining: &mut usize, raw_href: &str) -> String {
    let budget = *remaining;
    *remaining = 0;
    let head_len = budget / 2;
    let head = prefix_at(text, head_len);
    let tail = suffix_at(text, text.len().saturating_sub(budget - head_len));
    format!(
        "<pre>{}\n<span class=elided>[{} bytes elided — <a href=\"{}\">see raw</a>]</span>\n{}</pre>\n",
        esc(head),
        text.len().saturating_sub(head.len() + tail.len()),
        esc(raw_href),
        esc(tail)
    )
}

/// The longest prefix of `text` that is at most `max` bytes, cut on a
/// character boundary so the result is always a `&str`.
fn prefix_at(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The matching suffix, cut on a character boundary from the left.
fn suffix_at(text: &str, start: usize) -> &str {
    let mut begin = start.min(text.len());
    while !text.is_char_boundary(begin) {
        begin += 1;
    }
    &text[begin..]
}

/// Render a bounded Markdown subset after escaping all source text. Raw HTML
/// and off-host resources never become markup; links are limited to local
/// fragment/path targets so a reader cannot turn the page into a remote asset
/// loader.
fn render_markdown(markdown: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut fence_language = String::new();
    let mut code = String::new();
    let mut list_open = false;
    for line in markdown.lines() {
        if let Some(language) = line.strip_prefix("```") {
            if in_fence {
                out.push_str(&fenced_code(&code, &fence_language));
                code.clear();
                in_fence = false;
                fence_language.clear();
            } else {
                in_fence = true;
                fence_language = language.trim().to_string();
            }
            continue;
        }
        if in_fence {
            if !code.is_empty() {
                code.push('\n');
            }
            code.push_str(line);
            continue;
        }
        if line.trim().is_empty() {
            if list_open {
                out.push_str("</ul>\n");
                list_open = false;
            }
            continue;
        }
        if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            if !list_open {
                out.push_str("<ul>\n");
                list_open = true;
            }
            out.push_str(&format!("<li>{}</li>\n", render_inline(item)));
            continue;
        }
        if list_open {
            out.push_str("</ul>\n");
            list_open = false;
        }
        let heading = line.chars().take_while(|c| *c == '#').count();
        if (1..=3).contains(&heading) && line.chars().nth(heading) == Some(' ') {
            let text = line[heading + 1..].trim();
            out.push_str(&format!(
                "<h{heading}>{}</h{heading}>\n",
                render_inline(text)
            ));
        } else {
            out.push_str(&format!("<p>{}</p>\n", render_inline(line)));
        }
    }
    if in_fence {
        out.push_str(&fenced_code(&code, &fence_language));
    }
    if list_open {
        out.push_str("</ul>\n");
    }
    out
}

/// A fenced block, with the info string carried through as a class only when
/// one was written. An unclosed fence at the end of a message renders the same
/// way as a closed one rather than losing its language.
fn fenced_code(code: &str, language: &str) -> String {
    let class = if language.is_empty() {
        String::new()
    } else {
        format!(" class=\"language-{}\"", esc(language))
    };
    format!("<pre><code{class}>{}</code></pre>\n", esc(code))
}

fn render_inline(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let candidates = [
            (rest.find("**"), "**"),
            (rest.find('`'), "`"),
            (rest.find('['), "["),
        ];
        let Some((index, marker)) = candidates
            .iter()
            .filter_map(|(index, marker)| index.map(|index| (index, *marker)))
            .min_by_key(|(index, _)| *index)
        else {
            out.push_str(&esc(rest));
            break;
        };
        out.push_str(&esc(&rest[..index]));
        if marker == "**" {
            let after = &rest[index + 2..];
            if let Some(end) = after.find("**") {
                out.push_str("<strong>");
                out.push_str(&esc(&after[..end]));
                out.push_str("</strong>");
                rest = &after[end + 2..];
                continue;
            }
        } else if marker == "`" {
            let after = &rest[index + 1..];
            if let Some(end) = after.find('`') {
                out.push_str("<code>");
                out.push_str(&esc(&after[..end]));
                out.push_str("</code>");
                rest = &after[end + 1..];
                continue;
            }
        } else {
            let after = &rest[index + 1..];
            if let Some(label_end) = after.find("](") {
                let target = &after[label_end + 2..];
                if let Some((url, consumed)) = link_target(target) {
                    let label = &after[..label_end];
                    if is_local_target(url) {
                        out.push_str(&format!("<a href=\"{}\">{}</a>", esc(url), esc(label)));
                    } else {
                        // An off-host target is not followed and not hidden:
                        // the page loads no external resource and claims no
                        // authority over where the link went, so the label and
                        // the target are both printed as text.
                        out.push_str(&format!("{} ({})", esc(label), esc(url)));
                    }
                    rest = &target[consumed..];
                    continue;
                }
            }
        }
        out.push_str(&esc(marker));
        rest = &rest[index + marker.len()..];
    }
    out
}

/// Whether a Markdown target may become an `href` on this page, which claims
/// to load no off-host link.
///
/// A fragment and an absolute path are local. A target starting with `//` is
/// not a path: browsers read it as protocol-relative and resolve it against
/// the page's own scheme, so `//attacker.example` is off-host however much it
/// looks like one. A leading `\` is the same hole written differently, because
/// the URL parser maps `\` to `/` for the http(s) schemes — `/\attacker.example`
/// and `\\attacker.example` both reach that host once clicked. Testing only for
/// the first byte as `/` is what let these through.
fn is_local_target(url: &str) -> bool {
    if url.starts_with('#') {
        return true;
    }
    let Some(path) = url.strip_prefix('/') else {
        return false;
    };
    !path.starts_with('/') && !path.starts_with('\\')
}

/// The target inside `](…)` and how many bytes it occupied including the
/// closing `)`. Parentheses nest — a target may itself contain them — so
/// stopping at the first `)` would cut the URL in half and leave the rest of
/// it behind as stray text on the page.
fn link_target(after_open: &str) -> Option<(&str, usize)> {
    let mut depth = 0usize;
    for (offset, c) in after_open.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some((&after_open[..offset], offset + 1)),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}
