//! Shared JSON vocabulary for the `--json` output of the CLI commands.
//!
//! Every command that gains a `--json` flag promises the same thing: stdout
//! carries exactly one JSON object and nothing else. Inside that object the
//! repo's "never collapse unknown into zero / null / empty / absence" rule —
//! the same rule that shapes [`crate::activity::TimeSource`] — needs one
//! repeatable shape, and that shape lives here so `status`, `overview` and
//! `doctor` and their tests agree on it byte for byte.
//!
//! The tagged `kind` field is the discriminator, mirroring `TimeSource`:
//! a count is either `known` (a number, `0` meaning "looked and measured
//! empty"), `unknown` (with an explicit `why`), or `not_applicable` on this
//! machine. Serialising an unknown as `0`, `null`, `""` or an omitted field
//! is exactly the lie this repo has paid for before (`inbox.rs` `modified_ns`,
//! `runstate`'s pre-1970 clock), so these enums make the unknown explicit and
//! machine-readable instead.

use serde::Serialize;

/// A session count that is measured, unknown, or not applicable here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CountState {
    /// Measured and handed over. `count: 0` is a *measured* empty ("looked,
    /// nothing there"), which is a different claim from "never counted".
    Known { count: u64 },
    /// Could not be counted. The `why` says which of the distinct reasons
    /// (scan failed, store not enumerable, existence unprovable, ...).
    Unknown { why: String },
    /// The thing being counted does not apply on this machine (e.g. a registry
    /// cell for another platform).
    NotApplicable { why: String },
}

impl CountState {
    pub fn known(count: u64) -> Self {
        CountState::Known { count }
    }

    pub fn unknown(why: impl Into<String>) -> Self {
        CountState::Unknown { why: why.into() }
    }

    pub fn not_applicable(why: impl Into<String>) -> Self {
        CountState::NotApplicable { why: why.into() }
    }
}

/// A unix time that is either known or explicitly unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimeState {
    Known { unix: i64 },
    Unknown { why: String },
}

impl TimeState {
    pub fn known(unix: i64) -> Self {
        TimeState::Known { unix }
    }

    pub fn unknown(why: impl Into<String>) -> Self {
        TimeState::Unknown { why: why.into() }
    }
}
