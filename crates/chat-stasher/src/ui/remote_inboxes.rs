//! Local, content-free inbox observations, frozen at dashboard launch.
//! These counts describe inbound objects, never archived conversations.

use crate::inbox_status::{InboxView, Report};

use super::html::esc;

pub(super) fn json(report: &Report) -> serde_json::Value {
    serde_json::json!({
        "state": report.state,
        "complete": !report.incomplete(),
        "inboxes": report.inboxes,
    })
}

fn count(value: Option<usize>) -> String {
    value.map_or_else(|| "unknown".into(), |n| n.to_string())
}

fn instant(value: Option<u64>, history_state: &str) -> String {
    value.map_or_else(
        || {
            if history_state == "unknown" {
                "unknown"
            } else {
                "not recorded"
            }
            .into()
        },
        |t| format!("{t} (Unix seconds)"),
    )
}

fn row(inbox: &InboxView) -> String {
    let oldest = inbox.oldest_age_secs.map_or_else(
        || {
            if inbox.waiting == Some(0) {
                "not applicable".into()
            } else {
                "unknown".into()
            }
        },
        |secs| format!("{secs}s"),
    );
    let latest = match &inbox.last_pull {
        None => instant(None, inbox.history_state),
        Some(pull) => {
            let refused = pull.refused.as_ref().map_or_else(
                || "unknown".into(),
                |reasons| {
                    if reasons.is_empty() {
                        "none reported".into()
                    } else {
                        reasons
                            .iter()
                            .map(|(reason, n)| format!("{reason:?}: {n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                },
            );
            format!(
                "{}; state={}; exit_code={}; listed={}; stored={}; duplicates={}; missing account={}; refused={}",
                instant(Some(pull.at), inbox.history_state),
                match pull.state {
                    crate::inbox_status::PullState::Observed => "observed",
                    crate::inbox_status::PullState::Unknown => "unknown",
                },
                pull.exit_code, count(pull.listed), count(pull.stored),
                count(pull.duplicates), count(pull.missing_account), refused,
            )
        }
    };
    format!(
        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
        esc(&inbox.name),
        esc(inbox.backend_state),
        count(inbox.waiting),
        esc(&oldest),
        esc(inbox.history_state),
        esc(&instant(inbox.last_successful_pull, inbox.history_state)),
        esc(&latest),
    )
}

pub(super) fn render(report: &Report) -> String {
    let mut out = String::from(
        "<section><h2>Remote inboxes</h2>\n\
         <p>Local inbox observations at dashboard launch; waiting objects are not yet archived. \
         These observations are independent of destination and session filters. \
         Opaque waiting objects have no per-platform counts before pull.</p>\n",
    );
    if report.incomplete() {
        out.push_str("<p class=warn>Some inbox observations unknown; unavailable counts are unknown, not zero.</p>\n");
    }
    if report.inboxes.is_empty() && report.state == "known" {
        out.push_str("<p>No remote inboxes declared locally.</p>\n");
    }
    if !report.inboxes.is_empty() {
        out.push_str("<div class=scroll><table><thead><tr><th>Inbox</th><th>Backend</th><th>Waiting objects</th><th>Oldest age</th><th>History</th><th>Last successful pull</th><th>Latest pull attempt</th></tr></thead><tbody>\n");
        for inbox in &report.inboxes {
            out.push_str(&row(inbox));
        }
        out.push_str("</tbody></table></div>\n");
    }
    out.push_str("</section>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{inbox_config, inbox_status, remote_inbox::PullReport, ui};

    #[test]
    fn inbox_observations_preserve_absence_unknown_history_and_measured_zero() {
        let root = tempfile::tempdir().unwrap();
        let configs = root.path().join("synthetic-configs");
        let absent = inbox_status::inspect(&configs, 100);
        assert!(render(&absent).contains("No remote inboxes declared locally"));
        assert_eq!(json(&absent)["complete"], true);
        assert!(!configs.exists(), "observation must not create config");

        inbox_config::initialize(&configs, "synthetic-unreachable", "memory://synthetic").unwrap();
        let report = inbox_status::inspect(&configs, 100);
        let value = json(&report);
        assert_eq!(value["complete"], false);
        assert!(value["inboxes"][0]["waiting"].is_null());
        assert_eq!(value["inboxes"][0]["history_state"], "not_recorded");
        assert!(render(&report).contains("not recorded"));
        assert!(!render(&report).contains("No remote inboxes declared locally"));

        inbox_status::record(
            &configs,
            "synthetic-unreachable",
            100,
            Some(&PullReport::default()),
        )
        .unwrap();
        inbox_status::record(&configs, "synthetic-unreachable", 101, None).unwrap();
        let report = inbox_status::inspect(&configs, 110);
        let value = json(&report);
        assert_eq!(value["inboxes"][0]["last_successful_pull"], 100);
        assert_eq!(value["inboxes"][0]["last_pull"]["state"], "unknown");
        for key in [
            "listed",
            "stored",
            "duplicates",
            "missing_account",
            "refused",
        ] {
            assert!(value["inboxes"][0]["last_pull"][key].is_null());
        }
        assert!(render(&report).contains("refused=unknown"));
        assert!(render(&report).contains("stored=unknown"));

        let backend = root.path().join("synthetic-empty-backend");
        std::fs::create_dir(&backend).unwrap();
        let locator = format!("fs://{}", backend.display());
        inbox_config::initialize(&configs, "synthetic-empty", &locator).unwrap();
        let history_path = inbox_status::history_path(&configs, "synthetic-empty").unwrap();
        std::fs::write(&history_path, b"synthetic-corrupt-history").unwrap();
        let report = inbox_status::inspect(&configs, 110);
        let value = json(&report);
        assert_eq!(value["inboxes"][0]["waiting"], 0);
        assert_eq!(value["inboxes"][0]["history_state"], "unknown");
        assert!(value["inboxes"][0]["last_successful_pull"].is_null());
        assert_eq!(value["complete"], false);
        assert!(render(&report).contains("not applicable"));
        assert!(!render(&report).contains(&locator));
        assert_eq!(
            std::fs::read(&history_path).unwrap(),
            b"synthetic-corrupt-history"
        );

        let mut data = ui::fixture::data();
        let baseline: serde_json::Value =
            serde_json::from_str(&ui::json::json_overview(&data)).unwrap();
        data.remote_inboxes = report;
        let after: serde_json::Value =
            serde_json::from_str(&ui::json::json_overview(&data)).unwrap();
        assert_eq!(baseline["summary"], after["summary"]);
        assert_eq!(
            baseline["complete"], after["complete"],
            "archive completeness is independent"
        );
    }

    #[test]
    fn inbox_observations_escape_names_and_keep_bad_declarations_unknown() {
        let report = Report {
            state: "unknown",
            inboxes: vec![InboxView {
                name: "<synthetic>&name".into(),
                waiting: None,
                oldest_age_secs: None,
                backend_state: "unknown",
                history_state: "unknown",
                last_successful_pull: None,
                last_pull: None,
            }],
        };
        let html = render(&report);
        assert!(html.contains("&lt;synthetic&gt;&amp;name"));
        assert!(!html.contains("<synthetic>"));
        assert!(html.contains("inbox observations unknown"));
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("invalid-name!.json"),
            b"synthetic-invalid-config",
        )
        .unwrap();
        let report = inbox_status::inspect(root.path(), 100);
        assert_eq!(json(&report)["state"], "unknown");
        assert_eq!(json(&report)["complete"], false);
        assert!(!render(&report).contains("No remote inboxes declared locally"));
    }
}
