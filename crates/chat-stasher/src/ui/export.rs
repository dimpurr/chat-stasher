//! export — `/export`, the one route that hands a session to the browser as a
//! file rather than as a page (29-UI-DESIGN §4.7, R7).
//!
//! Everything here is decided by the rules the pages already follow:
//!
//! * **The bytes are not re-encoded.** The route asks the payload tier for the
//!   session — the same [`ContentSource::fetch`] `/content` and `/reader`
//!   use, which is `BackupStore::read_session_concat`, the one shard-joining
//!   rule `read` and the `export` command also share — and returns the
//!   concatenation exactly as the archive holds it. There is a second
//!   encoder nowhere: a downloaded file, `chat-stasher read` and
//!   `chat-stasher export --out` cannot disagree about a session's content,
//!   which is the property the design pins (29-UI-DESIGN §4.7: the body is
//!   `read`'s exact output, via the same fetch path).
//! * **The digest is stated twice**, the way it already is on `/content`:
//!   once in the `X-Checksum-Sha256` header so a downloader can verify the
//!   transfer, and once in the text around the download link (the cost box
//!   on `/session`, the CLI-parity block on `/sessions`). The value is
//!   [`Content::concat_sha256`], the same sha256 `read` prints for the
//!   concatenation.
//! * **The filename carries no conversation.** It is the session's
//!   privacy-safe short id (`<head>~<tag>.jsonl`), not a title and not the
//!   full id — a file name on a user's disk is the one place this project
//!   refuses to put either.
//! * **`fmt` is refused, not guessed.** `jsonl` is the only format v1
//!   serves; an unknown `fmt` is a 400 usage error for the same reason an
//!   unknown facet value is — a typo must not be dressed up as a download of
//!   something else.
//!
//! 29-UI-DESIGN §4.7 also says a session over 10 MB gets "/content-style
//! windowing + a download-continues link". `/content` has no windowing in this
//! tree (its route table entry for `w` is not implemented), so there is
//! nothing to be "the same as" yet; this route serves the whole session in one
//! response, bounded by the one fetch the other payload routes already pay.
//! When `/content` grows its window, the two must move together.
//!
//! Privacy line: this route emits conversation bytes, by design, to the one
//! browser session that already holds the page token. The responses' HTML
//! neighbours print only short ids, counts, byte sizes and digests.

use crate::view::Response;

use super::{bad_index_response, index_param, param, ContentSource, Query, UiData};

/// `application/octet-stream`, deliberately: with `Content-Disposition:
/// attachment` and the standing `X-Content-Type-Options: nosniff`, an
/// unspecified byte stream is all the browser should make of the file. Naming
/// a scriptable or markup type here would give the browser an interpretation
/// to perform on archive bytes, which is not what a download is for.
const ATTACHMENT_TYPE: &str = "application/octet-stream";

/// The export formats v1 serves, for the error message of an unknown `fmt`.
const FORMAT_VOCABULARY: &str = "jsonl (the only export format)";

pub(super) fn export_page(params: &Query, data: &UiData, content: &dyn ContentSource) -> Response {
    let Some(row) = index_param(params, data) else {
        return bad_index_response();
    };
    // Absent `fmt` is `jsonl` — the only shape the route has, so a bare
    // `/export?i=N` is unambiguous. Any *other* value is a typo or a request
    // for a format that does not exist, and neither may be answered as if it
    // had said `jsonl`.
    if let Some(other) = param(params, "fmt") {
        if other != "jsonl" {
            return Response::text(
                400,
                "Bad Request",
                format!(
                    "ui: `fmt` must be one of {FORMAT_VOCABULARY}, not `{other}`. An unknown \
                     format is a usage error, not an empty download.\n"
                ),
            );
        }
    }
    match content.fetch(&row.machine, &row.session_id) {
        Ok(c) => Response::bytes(200, "OK", ATTACHMENT_TYPE, c.concat)
            .with_header(
                "Content-Disposition",
                format!("attachment; filename=\"{}.jsonl\"", row.short_id),
            )
            // The digest the fetch itself computed — the same value `read`
            // prints and `/content` shows, never a second hash of the same
            // bytes.
            .with_header("X-Checksum-Sha256", c.concat_sha256),
        Err(e) => Response::text(
            502,
            "Bad Gateway",
            format!(
                "ui: could not read session {} on `{}` for export: {e}\n\
                 This is a failure to read, NOT an empty session.\n",
                row.short_id, row.machine
            ),
        ),
    }
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;
    use crate::view::Response;

    /// One request through the router, like the other ui modules' tests.
    fn req(target: &str, data: &crate::ui::UiData, src: &dyn ContentSource) -> Response {
        let (path, params) = crate::ui::split_target(target);
        crate::ui::handle(path, &params, "t", data, src, &crate::ui::NoIndex)
            .unwrap_or_else(|| panic!("`{target}` must be a known route"))
    }

    /// The payload these tests hand back: exact bytes, so every assertion is
    /// against content the route did not produce.
    const BODY: &[u8] = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"x\"}}\n";

    fn header<'a>(r: &'a Response, name: &str) -> &'a str {
        r.extra_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no {name} header"))
    }

    /// The core contract of §4.7: an attachment, byte-identical to what the
    /// fetch holds, named after the short id alone, with the digest beside it.
    #[test]
    fn export_returns_the_exact_bytes_as_an_attachment_with_the_digest() {
        struct Exact;
        impl ContentSource for Exact {
            fn fetch(
                &self,
                _machine: &str,
                _session_id: &str,
            ) -> Result<crate::ui::Content, String> {
                Ok(crate::ui::Content::from_concat(
                    BODY.to_vec(),
                    vec![("000001.jsonl".to_string(), "00".repeat(64))],
                ))
            }
        }
        let data = fixture::data();
        let r = req("/export?i=0", &data, &Exact);
        assert_eq!(r.status, 200);
        assert_eq!(r.body_bytes.as_deref(), Some(BODY), "no byte may move");
        // The X-Checksum-Sha256 must be the digest of exactly these bytes —
        // pinned here rather than trusted from the fixture, so the header
        // cannot become a second source of truth for the checksum.
        let want = {
            use sha2::{Digest, Sha256};
            Sha256::digest(BODY)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(header(&r, "X-Checksum-Sha256"), want);
        let dispo = header(&r, "Content-Disposition");
        assert!(
            dispo.starts_with("attachment; filename=\""),
            "it is an attachment, not a page: {dispo}"
        );
        // The filename is the short id of row 0 plus the format, and nothing
        // else — no full id, no title.
        assert_eq!(
            dispo,
            format!(
                "attachment; filename=\"{}.jsonl\"",
                data.session_at(0).unwrap().short_id
            ),
            "the session id must never reach the filename"
        );
        // …and the wire form carries the headers and the exact length.
        let wire = r.to_bytes();
        let head = String::from_utf8_lossy(&wire[..wire.len() - BODY.len()]);
        assert!(head.contains("Content-Disposition: attachment;"), "{head}");
        assert!(head.contains("X-Checksum-Sha256: "), "{head}");
        assert!(
            head.contains(&format!("Content-Length: {}\r\n", BODY.len())),
            "{head}"
        );
        assert!(
            head.contains("Content-Type: application/octet-stream"),
            "{head}"
        );
        assert_eq!(&wire[wire.len() - BODY.len()..], BODY);
    }

    /// `fmt=jsonl` is accepted, because it is the only format; anything else
    /// is refused as a usage error for the same reason an unknown facet is.
    #[test]
    fn unknown_formats_are_usage_errors_never_quiet_jsonl() {
        struct Exact;
        impl ContentSource for Exact {
            fn fetch(
                &self,
                _machine: &str,
                _session_id: &str,
            ) -> Result<crate::ui::Content, String> {
                Ok(crate::ui::Content::from_concat(BODY.to_vec(), Vec::new()))
            }
        }
        let d = fixture::data();
        assert_eq!(req("/export?i=1&fmt=jsonl", &d, &Exact).status, 200);
        for fmt in ["csv", "pdf", "JSONL", ""] {
            let r = req(&format!("/export?i=1&fmt={fmt}"), &d, &Exact);
            assert_eq!(r.status, 400, "`{fmt}` must not be served");
            assert!(
                r.body.contains("must be one of"),
                "the message must name the vocabulary: {}",
                r.body
            );
        }
    }

    /// An `i` that resolves to nothing is the row-family's own usage error,
    /// and no fetch is paid for it.
    #[test]
    fn an_unresolvable_index_is_a_usage_error_not_a_download() {
        struct Refuse;
        impl ContentSource for Refuse {
            fn fetch(
                &self,
                _machine: &str,
                _session_id: &str,
            ) -> Result<crate::ui::Content, String> {
                panic!("no index, no fetch")
            }
        }
        let d = fixture::data();
        for target in [
            "/export",
            "/export?i=99",
            "/export?i=x",
            "/export?fmt=jsonl",
        ] {
            let r = req(target, &d, &Refuse);
            assert_eq!(r.status, 400, "{target}");
            assert!(r.body.contains("`i` must name a row"), "{target}: {r:?}");
        }
    }

    /// A failure to read is a 502 that says so, never a zero-byte file: an
    /// empty download would claim the session held nothing (invariant 1).
    #[test]
    fn a_failed_read_is_a_502_not_an_empty_attachment() {
        struct Broken;
        impl ContentSource for Broken {
            fn fetch(
                &self,
                _machine: &str,
                _session_id: &str,
            ) -> Result<crate::ui::Content, String> {
                Err("shard blob missing".to_string())
            }
        }
        let r = req("/export?i=0&fmt=jsonl", &fixture::data(), &Broken);
        assert_eq!(r.status, 502);
        assert!(
            r.body.contains("failure to read, NOT an empty session"),
            "{:?}",
            r.body
        );
        assert!(
            r.extra_headers.is_empty(),
            "no download headers on a failure"
        );
    }
}
