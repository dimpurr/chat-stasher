//! Render scheduler templates without installing or registering them.
//!
//! The scheduler is deliberately external to chat-stasher: launchd/systemd
//! starts one run-once process, which exits after the pass. Rendering remains
//! side-effect free; the launchd install helpers are explicit and testable.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Days, Local, LocalResult, NaiveDate, TimeZone};
use clap::ValueEnum;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{Config, DEFAULT_BACKUP_INTERVAL_SECS};

pub const LAUNCHD_LABEL: &str = "com.chat-stasher.run-once";
pub const SYSTEMD_SERVICE: &str = "chat-stasher-run-once.service";
pub const SYSTEMD_TIMER: &str = "chat-stasher-run-once.timer";

/// Weekly `reclaim-stage` unit names. Deliberately separate from the run-once
/// names above: a launchd label / systemd unit name is the identity a restart
/// or teardown command refers to, so two timers must never share one.
pub const LAUNCHD_LABEL_RECLAIM_STAGE: &str = "com.chat-stasher.reclaim-stage";
pub const SYSTEMD_SERVICE_RECLAIM_STAGE: &str = "chat-stasher-reclaim-stage.service";
pub const SYSTEMD_TIMER_RECLAIM_STAGE: &str = "chat-stasher-reclaim-stage.timer";

/// The weekly `reclaim-stage` slot: Sunday 03:17 local time.
///
/// `reclaim-stage` proves every archived shard against its destination before
/// deleting the staged body — measured at ~20 min wall-clock, ~4% CPU, almost
/// entirely network (it reads the archive back). It is deliberately *not* on
/// the hourly run-once cadence.
///
/// Why Sunday: the whole previous week has been pushed by the hourly timer, so
/// a Sunday run reclaims a complete week of staged bodies before the next week
/// begins, and a personal machine is quietest at the weekend.
///
/// Why 03:17: quiet hours; and :17 deliberately avoids the :00/:15/:30/:45
/// minute marks that cron-style maintenance jobs cluster on. launchd has no
/// native jitter for `StartCalendarInterval`, so the fixed off-boundary minute
/// *is* the stagger lever; systemd gets real jitter via `RandomizedDelaySec`
/// ([`RECLAIM_STAGE_RANDOMIZED_DELAY_SECS`]). The hourly run-once uses
/// `StartInterval`, whose phase is relative to load time, so no single minute
/// is guaranteed collision-free — but a weekly ~20 minute network-bound pass
/// occasionally overlapping the hourly pass is a non-event compared with
/// running it on top of interactive work.
pub const RECLAIM_STAGE_WEEKDAY: u8 = 0; // launchd: 0 and 7 both mean Sunday.
pub const RECLAIM_STAGE_HOUR: u8 = 3;
pub const RECLAIM_STAGE_MINUTE: u8 = 17;

/// systemd `RandomizedDelaySec` for the weekly timer, in seconds: up to 15
/// minutes of random start delay so a fleet of machines does not all hit their
/// destination at 03:17:00 on the same second.
pub const RECLAIM_STAGE_RANDOMIZED_DELAY_SECS: u64 = 15 * 60;

/// Per-run scheduler jitter, in seconds. launchd has no random-delay key for
/// `StartInterval`, so the shell preamble sleeps for a bounded random interval
/// before `exec`; systemd gets the same bound through `RandomizedDelaySec`.
pub const SCHEDULER_RANDOMIZED_DELAY_SECS: u64 = 5 * 60;

/// Cap for the launchd stdout/stderr logs, in bytes. Beyond this the log is
/// truncated to empty in place at the start of the next run (see
/// [`render_launchd`]). macOS launchd has no rotation key of its own, so the
/// cap is enforced by the shell preamble we render into `ProgramArguments`.
pub const LAUNCHD_LOG_CAP_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Format {
    /// macOS launchd property list.
    Launchd,
    /// Linux systemd user service + timer.
    Systemd,
}

/// Which scheduled job the templates describe. `schedule` renders exactly one
/// job per invocation; the two units never share a launchd label or systemd
/// unit name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Unit {
    /// Hourly `run-once` archive cycle (the original behaviour).
    RunOnce,
    /// Weekly `reclaim-stage --apply` stage reclamation. This is stage
    /// reclamation (delete staged shard bodies once the archive proves it
    /// holds them) — unrelated to the ssh connection reaping that
    /// `--keep-ssh-masters` disables.
    ReclaimStage,
}

#[derive(Debug, Clone)]
pub struct TemplateFile {
    pub name: String,
    pub content: String,
}

/// Arguments forwarded from `schedule` to the embedded `run-once` command.
///
/// The template only renders what it is given; it does not decide whether a
/// destination is required. The caller (`cmd_schedule`) enforces the same
/// product rule as `run-once`.
#[derive(Debug, Clone, Default)]
pub struct RunOnceArgs {
    pub destination: Option<String>,
    pub repo: Option<String>,
    pub key_file: Option<String>,
    pub connections: Option<usize>,
    pub options: Vec<String>,
    pub machine: Option<String>,
    pub shard_bucket_cap: Option<usize>,
    pub keep_ssh_masters: bool,
    pub verify: bool,
}

/// Arguments forwarded from `schedule --unit reclaim-stage` to the embedded
/// `reclaim-stage` command. The stage path is separate — it is required in both
/// units — and `--apply` is always embedded (a weekly timer that only dry-ran
/// would report forever and delete nothing). The overrides here mirror
/// `reclaim-stage`'s own single-destination-only slots.
#[derive(Debug, Clone, Default)]
pub struct ReclaimStageArgs {
    pub repo: Option<String>,
    pub key_file: Option<String>,
    pub connections: Option<usize>,
    pub options: Vec<String>,
    pub keep_ssh_masters: bool,
}

/// Resolve the executable that a persistent scheduler may safely embed.
/// Cargo's `target/` paths are disposable build products, not installation
/// paths. An explicit non-build path is accepted without requiring it to exist
/// yet, so a package manager can render before its copy is complete.
pub fn resolve_binary(explicit: Option<&Path>, current_exe: &Path, home: &Path) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = absolute_path(path);
        if is_build_artifact(&path) {
            bail!(
                "binary path must be an installed path outside target/: {}",
                path.display()
            );
        }
        return Ok(path);
    }

    let current_exe = absolute_path(current_exe);
    if !is_build_artifact(&current_exe) {
        return Ok(current_exe);
    }

    let candidates = [
        home.join(".local/bin/chat-stasher"),
        PathBuf::from("/opt/homebrew/bin/chat-stasher"),
        PathBuf::from("/usr/local/bin/chat-stasher"),
    ];
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "current executable is a build artifact at {}; pass --binary with an installed path or install chat-stasher under ~/.local/bin or Homebrew",
                current_exe.display()
            )
        })
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn is_build_artifact(path: &Path) -> bool {
    let parts: Vec<&str> = path
        .iter()
        .filter_map(|component| component.to_str())
        .collect();
    parts
        .windows(2)
        .any(|window| window[0] == "target" && matches!(window[1], "debug" | "release"))
}

/// Resolve the configured cadence. Zero is rejected because it would create
/// a hot loop in both launchd and systemd.
pub fn interval_secs(config: &Config) -> Result<u64> {
    let interval = config
        .backup_interval_secs
        .unwrap_or(DEFAULT_BACKUP_INTERVAL_SECS);
    if interval == 0 {
        bail!("backup_interval_secs must be greater than zero");
    }
    Ok(interval)
}

/// Render the templates for one scheduled job. `unit` picks which job; the
/// run-once renderers take `args` (and `interval`), the reclaim-stage renderers
/// take `reclaim_args` (the weekly slot is fixed, so `interval` is unused there).
pub fn render(
    unit: Unit,
    format: Format,
    binary: &Path,
    stage: &Path,
    interval: u64,
    args: &RunOnceArgs,
    reclaim_args: &ReclaimStageArgs,
    home: &Path,
) -> Vec<TemplateFile> {
    match unit {
        Unit::RunOnce => render_run_once(format, binary, stage, interval, args, home),
        Unit::ReclaimStage => render_reclaim_stage(format, binary, stage, reclaim_args, home),
    }
}

fn render_run_once(
    format: Format,
    binary: &Path,
    stage: &Path,
    interval: u64,
    args: &RunOnceArgs,
    home: &Path,
) -> Vec<TemplateFile> {
    match format {
        Format::Launchd => vec![TemplateFile {
            name: format!(
                "{}.plist",
                launchd_label_for_destination(Unit::RunOnce, args.destination.as_deref())
            ),
            content: render_launchd(
                binary,
                stage,
                interval,
                args,
                home,
                &launchd_label_for_destination(Unit::RunOnce, args.destination.as_deref()),
            ),
        }],
        Format::Systemd => vec![
            TemplateFile {
                name: systemd_service_name_for_destination(
                    Unit::RunOnce,
                    args.destination.as_deref(),
                ),
                content: render_systemd_service(
                    binary,
                    stage,
                    args,
                    &systemd_service_name_for_destination(
                        Unit::RunOnce,
                        args.destination.as_deref(),
                    ),
                ),
            },
            TemplateFile {
                name: systemd_timer_name_for_destination(
                    Unit::RunOnce,
                    args.destination.as_deref(),
                ),
                content: render_systemd_timer(
                    interval,
                    &systemd_service_name_for_destination(
                        Unit::RunOnce,
                        args.destination.as_deref(),
                    ),
                ),
            },
        ],
    }
}

fn render_reclaim_stage(
    format: Format,
    binary: &Path,
    stage: &Path,
    args: &ReclaimStageArgs,
    home: &Path,
) -> Vec<TemplateFile> {
    match format {
        Format::Launchd => vec![TemplateFile {
            name: format!("{LAUNCHD_LABEL_RECLAIM_STAGE}.plist"),
            content: render_launchd_reclaim_stage(binary, stage, args, home),
        }],
        Format::Systemd => vec![
            TemplateFile {
                name: SYSTEMD_SERVICE_RECLAIM_STAGE.to_string(),
                content: render_systemd_service_reclaim_stage(binary, stage, args),
            },
            TemplateFile {
                name: SYSTEMD_TIMER_RECLAIM_STAGE.to_string(),
                content: render_systemd_timer_reclaim_stage(),
            },
        ],
    }
}

/// The launchd label for a unit. Pub so `cmd_schedule` can name the file a
/// saved plist should be copied to.
pub fn launchd_label(unit: Unit) -> &'static str {
    match unit {
        Unit::RunOnce => LAUNCHD_LABEL,
        Unit::ReclaimStage => LAUNCHD_LABEL_RECLAIM_STAGE,
    }
}

/// Return a collision-resistant, filesystem-safe launchd label for a named
/// destination. Legacy single-destination and reclaim-stage labels stay
/// unchanged so existing users can uninstall them.
pub fn launchd_label_for_destination(unit: Unit, destination: Option<&str>) -> String {
    let base = launchd_label(unit);
    match destination {
        Some(name) => format!("{base}.{}", destination_component(name)),
        None => base.to_string(),
    }
}

pub fn systemd_service_name(unit: Unit) -> &'static str {
    match unit {
        Unit::RunOnce => SYSTEMD_SERVICE,
        Unit::ReclaimStage => SYSTEMD_SERVICE_RECLAIM_STAGE,
    }
}

pub fn systemd_service_name_for_destination(unit: Unit, destination: Option<&str>) -> String {
    let base = systemd_service_name(unit);
    match destination {
        Some(name) => base.replace(
            ".service",
            &format!("-{}.service", destination_component(name)),
        ),
        None => base.to_string(),
    }
}

pub fn systemd_timer_name(unit: Unit) -> &'static str {
    match unit {
        Unit::RunOnce => SYSTEMD_TIMER,
        Unit::ReclaimStage => SYSTEMD_TIMER_RECLAIM_STAGE,
    }
}

pub fn systemd_timer_name_for_destination(unit: Unit, destination: Option<&str>) -> String {
    let base = systemd_timer_name(unit);
    match destination {
        Some(name) => base.replace(".timer", &format!("-{}.timer", destination_component(name))),
        None => base.to_string(),
    }
}

fn destination_component(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if safe == name && !safe.is_empty() {
        return safe;
    }
    let digest = Sha256::digest(name.as_bytes());
    let suffix = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{suffix}",
        if safe.is_empty() {
            "destination"
        } else {
            &safe
        }
    )
}

/// Write rendered templates. For launchd, output is the plist file. For
/// systemd, output is a directory containing the service and timer files.
pub fn write_templates(
    format: Format,
    output: &Path,
    files: &[TemplateFile],
) -> Result<Vec<PathBuf>> {
    if matches!(format, Format::Systemd) || files.len() > 1 {
        fs::create_dir_all(output)
            .with_context(|| format!("create scheduler template directory {}", output.display()))?;
    } else if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create launchd template directory {}", parent.display()))?;
    }

    let mut paths = Vec::with_capacity(files.len());
    for file in files {
        let path = if matches!(format, Format::Systemd) || files.len() > 1 {
            output.join(&file.name)
        } else {
            output.to_path_buf()
        };
        fs::write(&path, &file.content)
            .with_context(|| format!("write scheduler template {}", path.display()))?;
        paths.push(path);
    }
    Ok(paths)
}

pub fn install_command(unit: Unit, format: Format, paths: &[PathBuf]) -> String {
    match format {
        Format::Launchd => {
            let mut command = String::from(
                "mkdir -p \"$HOME/Library/LaunchAgents\" \"$HOME/Library/Logs/chat-stasher\"",
            );
            for path in paths {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{}.plist", launchd_label(unit)));
                command.push_str(&format!(
                    " && cp {} \"$HOME/Library/LaunchAgents/{name}\" && launchctl bootstrap \"gui/$(id -u)\" \"$HOME/Library/LaunchAgents/{name}\"",
                    shell_quote(path)
                ));
            }
            if paths.is_empty() {
                command.push_str(" && # generated plist path missing");
            }
            command
        }
        Format::Systemd => {
            // A destination-specific render names its units after the
            // destination, so the installed target names must come from the
            // files themselves, not from the unit's fixed default names. With
            // several destinations there is one service+timer pair per
            // destination, so every printed file is installed and every timer
            // is enabled — not only the first pair.
            if paths.is_empty() {
                return format!(
                    "systemctl --user daemon-reload && systemctl --user enable --now {}",
                    systemd_timer_name(unit)
                );
            }
            let mut command = String::new();
            for path in paths {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                command.push_str(&format!(
                    "install -Dm644 {} \"$HOME/.config/systemd/user/{name}\" && ",
                    shell_quote(path)
                ));
            }
            command.push_str("systemctl --user daemon-reload");
            for path in paths {
                let Some(name) = path.file_name() else {
                    continue;
                };
                let name = name.to_string_lossy();
                if name.ends_with(".timer") {
                    command.push_str(&format!(" && systemctl --user enable --now {name}"));
                }
            }
            command
        }
    }
}

/// Installation command for templates that render-only mode only *printed* by
/// name. One destination yields one plist (launchd) or one service+timer pair
/// (systemd), so the command names every printed file instead of assuming the
/// single fixed unit name.
pub fn install_command_for_files(format: Format, files: &[TemplateFile]) -> String {
    match format {
        Format::Launchd => {
            let mut command = String::from(
                "mkdir -p \"$HOME/Library/LaunchAgents\" \"$HOME/Library/Logs/chat-stasher\"",
            );
            for file in files {
                command.push_str(&format!(
                    " && launchctl bootstrap \"gui/$(id -u)\" \"$HOME/Library/LaunchAgents/{name}\"",
                    name = file.name
                ));
            }
            command
        }
        Format::Systemd => {
            let mut command = String::from("systemctl --user daemon-reload");
            for file in files.iter().filter(|file| file.name.ends_with(".timer")) {
                command.push_str(&format!(
                    " && systemctl --user enable --now {name}",
                    name = file.name
                ));
            }
            command
        }
    }
}

/// The launchctl executable is passed in by the caller so tests can use a
/// throwaway fake without touching the machine's real launchd session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallResult {
    Installed,
    Unchanged,
}

pub fn install_launchd_agents(
    home: &Path,
    files: &[TemplateFile],
    launchctl: &Path,
    domain: &str,
) -> Result<Vec<InstallResult>> {
    let agents = home.join("Library/LaunchAgents");
    fs::create_dir_all(&agents)
        .with_context(|| format!("create launchd agent directory {}", agents.display()))?;
    fs::create_dir_all(home.join("Library/Logs/chat-stasher"))
        .with_context(|| format!("create launchd log directory under {}", home.display()))?;

    let mut results = Vec::with_capacity(files.len());
    for file in files {
        let path = agents.join(&file.name);
        let label = file.name.strip_suffix(".plist").unwrap_or(&file.name);
        let target = format!("{domain}/{label}");
        // A read error means the installed plist is absent or unreadable, which
        // is not the same bytes as what we would write. The safe direction is
        // to rewrite and reload, never to leave a stale unit in place.
        let same = fs::read(&path)
            .map(|bytes| bytes == file.content.as_bytes())
            .unwrap_or(false); // reason: an absent or unreadable installed plist is treated as changed, so install rewrites and reloads it
        let loaded = launchctl_status(launchctl, &target)?;
        if same && loaded {
            results.push(InstallResult::Unchanged);
            continue;
        }
        if loaded {
            launchctl_run(launchctl, &["bootout", domain, label])
                .with_context(|| format!("unload launchd agent {label}"))?;
        }
        if !same {
            write_atomic(&path, file.content.as_bytes())
                .with_context(|| format!("write launchd agent {}", path.display()))?;
        }
        launchctl_run(launchctl, &["bootstrap", domain, &path.to_string_lossy()])
            .with_context(|| format!("load launchd agent {label}"))?;
        results.push(InstallResult::Installed);
    }
    Ok(results)
}

pub fn uninstall_launchd_agents(
    home: &Path,
    labels: &[String],
    launchctl: &Path,
    domain: &str,
) -> Result<usize> {
    let agents = home.join("Library/LaunchAgents");
    let mut removed = 0;
    for label in labels {
        let path = agents.join(format!("{label}.plist"));
        let target = format!("{domain}/{label}");
        if launchctl_status(launchctl, &target)? {
            launchctl_run(launchctl, &["bootout", domain, label])
                .with_context(|| format!("unload launchd agent {label}"))?;
        }
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("remove launchd agent {}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn install_systemd_units(
    home: &Path,
    files: &[TemplateFile],
    systemctl: &Path,
) -> Result<Vec<InstallResult>> {
    let units = home.join(".config/systemd/user");
    fs::create_dir_all(&units)
        .with_context(|| format!("create systemd user unit directory {}", units.display()))?;
    let mut changed = false;
    let mut timers = Vec::new();
    for file in files {
        let path = units.join(&file.name);
        let same = fs::read(&path)
            .map(|bytes| bytes == file.content.as_bytes())
            .unwrap_or(false); // reason: absent or unreadable units must be rewritten before they can be enabled
        if !same {
            write_atomic(&path, file.content.as_bytes())
                .with_context(|| format!("write systemd unit {}", path.display()))?;
            changed = true;
        }
        if file.name.ends_with(".timer") {
            timers.push(file.name.clone());
        }
    }
    systemctl_run(systemctl, &["--user", "daemon-reload"])?;
    let mut active = true;
    for timer in &timers {
        let output = Command::new(systemctl)
            .args(["--user", "is-active", "--quiet", timer])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("check systemd timer {timer}"))?;
        active &= output.success();
    }
    if !active || changed {
        for timer in &timers {
            systemctl_run(systemctl, &["--user", "enable", "--now", timer])?;
        }
    }
    Ok(vec![if changed || !active {
        InstallResult::Installed
    } else {
        InstallResult::Unchanged
    }])
}

pub fn uninstall_systemd_units(home: &Path, timers: &[String], systemctl: &Path) -> Result<usize> {
    let units = home.join(".config/systemd/user");
    let mut removed = 0;
    for timer in timers {
        let timer_path = units.join(timer);
        if timer_path.exists() {
            systemctl_run(systemctl, &["--user", "disable", "--now", timer])?;
        }
        for name in [
            timer
                .strip_suffix(".timer")
                .map(|stem| format!("{stem}.service")),
            Some(timer.clone()),
        ]
        .into_iter()
        .flatten()
        {
            let path = units.join(name);
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("remove systemd unit {}", path.display()))?;
                removed += 1;
            }
        }
    }
    systemctl_run(systemctl, &["--user", "daemon-reload"])?;
    Ok(removed)
}

/// What the scheduler says about when the job it was given runs next.
///
/// Two states, never one. [`NextRun::Known`] carries a time that came from a
/// source that knows it — systemd's own answer for a timer, or the local time
/// a launchd `StartCalendarInterval` resolves to — and is never derived from
/// the cadence. [`NextRun::Unknown`] carries the sentence that travels with
/// the empty value: a bare `next_run: null` reads as "there is none", which is
/// a different claim from "nobody could say".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextRun {
    Known(String),
    Unknown(String),
}

impl NextRun {
    /// The timestamp, or `None` when no source that knows one has spoken.
    pub fn value(&self) -> Option<&str> {
        match self {
            NextRun::Known(value) => Some(value),
            NextRun::Unknown(_) => None,
        }
    }

    /// Why there is no timestamp. `Some` exactly when [`NextRun::value`] is
    /// `None`, so the empty value never reaches a reader without its reason.
    pub fn note(&self) -> Option<&str> {
        match self {
            NextRun::Known(_) => None,
            NextRun::Unknown(note) => Some(note),
        }
    }
}

/// The destinations whose units an install or an uninstall touches: the ones
/// named on the command line, or every declared destination when none were
/// named. One unit per destination is the shape both renderers produce, so
/// this is also the set of units whose next run has to be asked about —
/// `declared` is expected in a stable (sorted) order so the same config yields
/// the same set of units on every call.
pub fn install_targets(declared: &[String], requested: &[String]) -> Vec<Option<String>> {
    if !requested.is_empty() {
        return requested.iter().cloned().map(Some).collect();
    }
    if declared.is_empty() {
        // No destination at all is still one unit: both renderers fall back to
        // the destination-less names (see `launchd_label_for_destination`).
        return vec![None];
    }
    declared.iter().cloned().map(Some).collect()
}

/// Ask the scheduler for the next run of the units that were just installed.
///
/// Nothing here is estimated. systemd is asked through `systemctl --user
/// status`, whose `Trigger:` line is formatted from the same
/// `calc_next_elapse` value `list-timers` prints as `NEXT` — including the
/// `RandomizedDelaySec` offset, which systemd folds into `NextElapseUSec*`
/// before arming — and the value is passed through verbatim, because
/// reformatting it would mean being right about a clock this code cannot see.
/// The `NEXT` column itself is text with no fixed width (it may carry a
/// timezone abbreviation) and `--output=json` does not exist for
/// `list-timers` upstream, so the delimited `Trigger:` line is the only
/// machine-readable spelling of the same value. A launchd `StartInterval` job
/// is a relative interval measured from the moment the job was loaded, not a
/// wall-clock deadline, so there is no time to report for it; a
/// `StartCalendarInterval` job is anchored to calendar fields, and the plist
/// is the source: the slot is computed on the local calendar, which is the
/// clock launchd itself reads.
///
/// `systemctl` is only run by the systemd probe; the launchd probe reads the
/// installed plist instead.
pub fn next_run(
    unit: Unit,
    format: Format,
    targets: &[Option<String>],
    home: &Path,
    systemctl: &Path,
    now: DateTime<Local>,
) -> NextRun {
    match targets {
        [] => NextRun::Unknown(
            "no scheduler unit was installed, so there is no next run to report".to_string(),
        ),
        [destination] => match format {
            Format::Launchd => launchd_next_run(unit, destination.as_deref(), home, now),
            Format::Systemd => systemd_next_run(unit, destination.as_deref(), systemctl),
        },
        // Each destination has its own timer, and this report carries one next
        // run. Naming one of them would be a claim about the others, and
        // comparing them is worse: their timestamps are the scheduler's own
        // text, so "earliest" would be a string comparison across two possibly
        // different formats and timezones.
        _ => NextRun::Unknown(format!(
            "{} destinations have one timer each and this report carries a single next run; ask \
             the scheduler about one timer at a time",
            targets.len()
        )),
    }
}

/// The `Trigger:` line `systemctl status` prints for a timer's next run.
///
/// The line reads `    Trigger: <timestamp>; <relative time>`. The `; ` is the
/// only delimiter that holds: the timestamp carries a timezone abbreviation
/// (`... 03:17:00 CEST`), so it has no fixed width, and what follows it is
/// prose. `n/a` is systemd's own spelling for "no next elapse" and is reported
/// as nothing rather than as a time.
fn systemd_trigger_line(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("Trigger:")?.trim();
        let value = rest.split("; ").next().unwrap_or(rest).trim();
        if value.is_empty() || value == "n/a" {
            return None;
        }
        Some(value.to_string())
    })
}

fn systemd_next_run(unit: Unit, destination: Option<&str>, systemctl: &Path) -> NextRun {
    let timer = systemd_timer_name_for_destination(unit, destination);
    let output = Command::new(systemctl)
        .args(["--user", "--no-pager", "status", &timer])
        .output();
    match output {
        Err(error) => NextRun::Unknown(format!(
            "{} could not be run to ask for {timer}'s next run ({error}), so it is unknown",
            systemctl.display()
        )),
        Ok(output) => match systemd_trigger_line(&String::from_utf8_lossy(&output.stdout)) {
            Some(value) => NextRun::Known(value),
            None => NextRun::Unknown(format!(
                "systemd did not report a next run for {timer} (it prints `Trigger: n/a` until \
                 the timer is armed); `systemctl --user list-timers` shows the timer's state"
            )),
        },
    }
}

fn launchd_next_run(
    unit: Unit,
    destination: Option<&str>,
    home: &Path,
    now: DateTime<Local>,
) -> NextRun {
    let label = launchd_label_for_destination(unit, destination);
    let path = home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return NextRun::Unknown(format!(
                "the installed plist {} could not be read ({error}), so the next fire time is \
                 unknown",
                path.display()
            ))
        }
    };
    if let Some(interval) = plist_integer(&text, "StartInterval") {
        // A StartInterval is relative to the moment the job was loaded, so an
        // absolute deadline would have to be measured from a load time this
        // process never saw — and launchd does not expose one either.
        return NextRun::Unknown(format!(
            "launchd interval jobs expose no next fire time; the job runs {} after load",
            interval_phrase(interval)
        ));
    }
    let Some(slot) = plist_calendar_slot(&text) else {
        return NextRun::Unknown(format!(
            "the installed plist {} declares neither StartInterval nor a StartCalendarInterval \
             naming Weekday, Hour and Minute, so no next fire time can be derived from it",
            path.display()
        ));
    };
    match next_calendar_occurrence(slot, now) {
        Some(when) => NextRun::Known(when.format("%a %Y-%m-%d %H:%M:%S %z").to_string()),
        None => NextRun::Unknown(
            "the next occurrence of the plist's calendar slot does not exist in local time (a \
             daylight-saving gap), so no fire time can be reported"
                .to_string(),
        ),
    }
}

/// An integer-valued plist key: the first `<integer>` after `<key>NAME</key>`.
fn plist_integer(text: &str, key: &str) -> Option<u64> {
    let after_key = text.split_once(&format!("<key>{key}</key>"))?.1;
    let after_open = after_key.split_once("<integer>")?.1;
    let (value, _) = after_open.split_once("</integer>")?;
    value.trim().parse().ok()
}

/// The `Weekday` / `Hour` / `Minute` of a launchd `StartCalendarInterval`.
///
/// The dictionary the renderer writes is flat, so the text between the key and
/// the next `</dict>` is unambiguous. A value outside the range launchd
/// accepts is treated as unreadable rather than clamped: clamping would move
/// the fire time to one nobody asked for.
fn plist_calendar_slot(text: &str) -> Option<CalendarSlot> {
    let after_key = text.split_once("<key>StartCalendarInterval</key>")?.1;
    let dict = after_key.split_once("</dict>")?.0;
    let weekday = u32::try_from(plist_integer(dict, "Weekday")?).ok()?;
    let hour = u32::try_from(plist_integer(dict, "Hour")?).ok()?;
    let minute = u32::try_from(plist_integer(dict, "Minute")?).ok()?;
    if weekday > 7 || hour > 23 || minute > 59 {
        return None;
    }
    Some(CalendarSlot {
        weekday,
        hour,
        minute,
    })
}

struct CalendarSlot {
    weekday: u32,
    hour: u32,
    minute: u32,
}

/// The next occurrence of a launchd calendar slot, on the local calendar.
///
/// launchd fires `StartCalendarInterval` at local wall-clock time, so this is
/// civil arithmetic: the answer is the wall clock launchd itself reads, with
/// no timezone conversion and no DST rule of ours to get wrong. A slot that
/// does not exist in local time (inside a spring-forward gap) is reported as
/// nothing rather than moved, because moving it would state a fire time
/// launchd was never asked for.
fn next_calendar_occurrence(slot: CalendarSlot, now: DateTime<Local>) -> Option<DateTime<Local>> {
    // launchd accepts 0 and 7 for Sunday; chrono counts from Sunday at 0.
    let target = slot.weekday % 7;
    let today = now.date_naive();
    let day = today.checked_add_days(Days::new(u64::from(
        (target + 7 - today.weekday().num_days_from_sunday()) % 7,
    )))?;
    match local_at(day, &slot)? {
        this_week if this_week > now => Some(this_week),
        _ => local_at(day.checked_add_days(Days::new(7))?, &slot),
    }
}

/// `date` at the slot's wall-clock time in the local zone.
fn local_at(date: NaiveDate, slot: &CalendarSlot) -> Option<DateTime<Local>> {
    let naive = date.and_hms_opt(slot.hour, slot.minute, 0)?;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(when) => Some(when),
        // A repeated wall-clock time (the autumn fall-back) happens twice; the
        // earlier one is the one that comes next.
        LocalResult::Ambiguous(first, _) => Some(first),
        LocalResult::None => None,
    }
}

/// A duration in the unit that keeps it a whole number, as prose: `every 60
/// minutes`, `every 90 seconds`. Rounding it to a friendlier unit would make
/// the sentence describe a cadence the plist does not declare.
fn interval_phrase(interval: u64) -> String {
    let (value, unit) = if interval % 60 == 0 {
        (interval / 60, "minute")
    } else {
        (interval, "second")
    };
    if value == 1 {
        format!("every {value} {unit}")
    } else {
        format!("every {value} {unit}s")
    }
}

fn systemctl_run(systemctl: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(systemctl)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run {}", systemctl.display()))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{} exited with status {}", systemctl.display(), status)
    }
}

fn launchctl_status(launchctl: &Path, target: &str) -> Result<bool> {
    let status = Command::new(launchctl)
        .arg("print")
        .arg(target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run {} print", launchctl.display()))?;
    Ok(status.success())
}

fn launchctl_run(launchctl: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(launchctl)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run {}", launchctl.display()))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{} exited with status {}", launchctl.display(), status)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("plist.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Best-effort cleanup of the temporary file; the rename error is
            // the one that must be reported.
            drop(fs::remove_file(&tmp));
            Err(error)
        }
    }
}

/// Build the `run-once` argument vector that both launchd and systemd will
/// embed. Paths and values are kept as separate tokens so each renderer can
/// quote them in its own dialect.
fn run_once_argv(binary: &Path, stage: &Path, args: &RunOnceArgs) -> Vec<String> {
    let mut argv = vec![
        binary.to_string_lossy().into_owned(),
        "run-once".to_string(),
        "--stage".to_string(),
        stage.to_string_lossy().into_owned(),
    ];
    if let Some(machine) = &args.machine {
        argv.push("--machine".to_string());
        argv.push(machine.clone());
    }
    if let Some(cap) = args.shard_bucket_cap {
        argv.push("--shard-bucket-cap".to_string());
        argv.push(cap.to_string());
    }
    if let Some(destination) = &args.destination {
        argv.push("--destination".to_string());
        argv.push(destination.clone());
    }
    if let Some(repo) = &args.repo {
        argv.push("--repo".to_string());
        argv.push(repo.clone());
    }
    if let Some(key_file) = &args.key_file {
        argv.push("--key-file".to_string());
        argv.push(key_file.clone());
    }
    if let Some(connections) = args.connections {
        argv.push("--connections".to_string());
        argv.push(connections.to_string());
    }
    for opt in &args.options {
        argv.push("--option".to_string());
        argv.push(opt.clone());
    }
    if args.verify {
        argv.push("--verify".to_string());
    }
    if args.keep_ssh_masters {
        argv.push("--keep-ssh-masters".to_string());
    }
    argv
}

/// Build the `reclaim-stage` argument vector that both launchd and systemd embed.
/// The weekly timer is *always* an apply: a timer that only dry-ran would
/// report forever and delete nothing. Paths and values are kept as separate
/// tokens so each renderer can quote them in its own dialect.
fn reclaim_stage_argv(binary: &Path, stage: &Path, args: &ReclaimStageArgs) -> Vec<String> {
    let mut argv = vec![
        binary.to_string_lossy().into_owned(),
        "reclaim-stage".to_string(),
        "--stage".to_string(),
        stage.to_string_lossy().into_owned(),
        "--apply".to_string(),
    ];
    if let Some(repo) = &args.repo {
        argv.push("--repo".to_string());
        argv.push(repo.clone());
    }
    if let Some(key_file) = &args.key_file {
        argv.push("--key-file".to_string());
        argv.push(key_file.clone());
    }
    if let Some(connections) = args.connections {
        argv.push("--connections".to_string());
        argv.push(connections.to_string());
    }
    for opt in &args.options {
        argv.push("--option".to_string());
        argv.push(opt.clone());
    }
    if args.keep_ssh_masters {
        argv.push("--keep-ssh-masters".to_string());
    }
    argv
}

fn render_launchd(
    binary: &Path,
    stage: &Path,
    interval: u64,
    args: &RunOnceArgs,
    home: &Path,
    label: &str,
) -> String {
    let argv = run_once_argv(binary, stage, args);

    let log_dir = home.join("Library/Logs/chat-stasher");
    let stdout_log = log_dir.join("run-once.log");
    let stderr_log = log_dir.join("run-once.err.log");

    // launchd opens StandardOutPath/StandardErrorPath with O_APPEND *before*
    // our process starts, and its fd follows the inode, not the path. Renaming
    // either log at startup would therefore strand this run's output in the
    // renamed file. So we cap by truncating *in place* — the one mutation that
    // keeps launchd's fd valid — instead of rotating: if a log exceeds the cap
    // we truncate it to empty, then `exec` the real binary. Truncation never
    // changes the inode and O_APPEND resumes writing at the new end; `exec`
    // keeps launchd's view of our exit status intact (the tracked process *is*
    // chat-stasher, so SuccessExitStatus=0 semantics are preserved).
    let cap = LAUNCHD_LOG_CAP_BYTES;
    let command = format!(
        "{}\n{}\njitter=$(( $(od -An -N2 -tu2 /dev/urandom) % {} ))\nsleep \"$jitter\"\nexec {}",
        cap_line(&stdout_log, cap),
        cap_line(&stderr_log, cap),
        SCHEDULER_RANDOMIZED_DELAY_SECS + 1,
        argv.iter()
            .map(|arg| sh_single_quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
    );

    let arguments = ["/bin/sh", "-c", &command]
        .iter()
        .map(|arg| format!("        <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- chat-stasher is a one-shot process: exit 0 is success; result=NOOP means no snapshot, result=COMPLETED means snapshot created; non-zero is error. -->
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}
  </array>
  <key>StartInterval</key>
  <integer>{interval}</integer>
  <key>RunAtLoad</key>
  <false/>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#,
        stdout = xml_escape(&stdout_log.to_string_lossy()),
        stderr = xml_escape(&stderr_log.to_string_lossy()),
        label = xml_escape(label),
    )
}

fn render_launchd_reclaim_stage(
    binary: &Path,
    stage: &Path,
    args: &ReclaimStageArgs,
    home: &Path,
) -> String {
    let argv = reclaim_stage_argv(binary, stage, args);

    let log_dir = home.join("Library/Logs/chat-stasher");
    let stdout_log = log_dir.join("reclaim-stage.log");
    let stderr_log = log_dir.join("reclaim-stage.err.log");

    // Same in-place-truncate cap as run-once: launchd opens the log fds before
    // our process starts and its fds follow the inode, so truncating *in place*
    // is the one mutation that keeps this run's output attached. `exec` keeps
    // launchd's view of the exit status intact.
    let cap = LAUNCHD_LOG_CAP_BYTES;
    let command = format!(
        "{}\n{}\nexec {}",
        cap_line(&stdout_log, cap),
        cap_line(&stderr_log, cap),
        argv.iter()
            .map(|arg| sh_single_quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
    );

    let arguments = ["/bin/sh", "-c", &command]
        .iter()
        .map(|arg| format!("        <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- chat-stasher reclaim-stage is a weekly one-shot: exit 0 is success (reclaimed, or nothing to reclaim); exit 1 means BLOCKED (a destination could not be proven) and nothing was deleted; non-zero is error. -->
  <key>Label</key>
  <string>{LAUNCHD_LABEL_RECLAIM_STAGE}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}
  </array>
  <!-- Sunday 03:17 local; :17 dodges the :00/:15/:30/:45 minutes cron-style jobs cluster on (launchd has no StartCalendarInterval jitter of its own, so the off-boundary minute is the stagger lever). See RECLAIM_STAGE_HOUR / RECLAIM_STAGE_MINUTE. -->
  <key>StartCalendarInterval</key>
  <dict>
    <key>Weekday</key>
    <integer>{RECLAIM_STAGE_WEEKDAY}</integer>
    <key>Hour</key>
    <integer>{RECLAIM_STAGE_HOUR}</integer>
    <key>Minute</key>
    <integer>{RECLAIM_STAGE_MINUTE}</integer>
  </dict>
  <key>RunAtLoad</key>
  <false/>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#,
        stdout = xml_escape(&stdout_log.to_string_lossy()),
        stderr = xml_escape(&stderr_log.to_string_lossy()),
    )
}

/// One line of the launchd shell preamble: truncate `path` to empty if it
/// exceeds `cap`. Written as a POSIX `sh` test-and-truncate so a missing or
/// otherwise odd file can never abort the run before `exec`.
fn cap_line(path: &Path, cap: usize) -> String {
    format!(
        "f={path}; [ -f \"$f\" ] && [ \"$(stat -f%z \"$f\")\" -gt {cap} ] && : > \"$f\"",
        path = sh_single_quote(&path.to_string_lossy())
    )
}

/// One value spelled so a POSIX `sh` reading it sees exactly the value and
/// nothing else: wrapped in single-quotes (XCU §2.2.2, where every character
/// is literal), and a single quote in the input is closed, re-opened around
/// a double-quoted quote, and continued (`'"'"'` — the `'` is literal inside
/// double-quotes, XCU §2.2.3). Shared by the cron/launchd command lines here
/// and by the UI's copyable `chat-stasher export` command, so the binary
/// carries one quoting idiom, not two. Quoting is unconditional: deciding
/// per value whether it "looks safe" is the renderer judging input it does
/// not control.
pub(crate) fn sh_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn render_systemd_service(
    binary: &Path,
    stage: &Path,
    args: &RunOnceArgs,
    service_name: &str,
) -> String {
    let argv = run_once_argv(binary, stage, args);
    let args = argv
        .iter()
        .map(|arg| systemd_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"[Unit]
Description=Run one chat-stasher archive cycle ({service_name})

[Service]
Type=oneshot
# exit 0 = success: result=NOOP means no snapshot; result=COMPLETED means snapshot created.
# Non-zero = error; read the result line in the journal.
ExecStart={args}
SuccessExitStatus=0
StandardOutput=journal
StandardError=journal
"#
    )
}

fn render_systemd_timer(interval: u64, service_name: &str) -> String {
    format!(
        r#"[Unit]
Description=Hourly chat-stasher archive cycle

[Timer]
OnBootSec={interval}s
OnUnitActiveSec={interval}s
RandomizedDelaySec={delay}s
Persistent=true
Unit={service_name}

[Install]
WantedBy=timers.target
"#,
        delay = SCHEDULER_RANDOMIZED_DELAY_SECS,
        service_name = service_name,
    )
}

fn render_systemd_service_reclaim_stage(
    binary: &Path,
    stage: &Path,
    args: &ReclaimStageArgs,
) -> String {
    let argv = reclaim_stage_argv(binary, stage, args);
    let args = argv
        .iter()
        .map(|arg| systemd_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"[Unit]
Description=Reclaim chat-stasher stage shards proven by the archive (weekly)

[Service]
Type=oneshot
# exit 0 = success: reclaimed, or nothing to reclaim. exit 1 = BLOCKED: a
# destination could not be proven, so nothing was deleted. Non-zero = error.
# Read the result line in the journal.
ExecStart={args}
SuccessExitStatus=0
StandardOutput=journal
StandardError=journal
"#
    )
}

fn render_systemd_timer_reclaim_stage() -> String {
    format!(
        r#"[Unit]
Description=Weekly chat-stasher stage reclamation

[Timer]
# Sunday 03:17 local. RandomizedDelaySec adds up to 15 min of jitter so a
# fleet does not all hit their destination at 03:17:00 on the same second;
# Persistent catches up a missed Sunday (e.g. laptop asleep) on next wake.
OnCalendar=Sun *-*-* 03:17:00
RandomizedDelaySec={delay}s
Persistent=true
Unit={SYSTEMD_SERVICE_RECLAIM_STAGE}

[Install]
WantedBy=timers.target
"#,
        delay = RECLAIM_STAGE_RANDOMIZED_DELAY_SECS,
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn systemd_quote_arg(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('%', "%%")
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_interval_is_hourly() {
        assert_eq!(interval_secs(&Config::default()).unwrap(), 3600);
    }

    #[test]
    fn zero_interval_is_rejected() {
        let config = Config {
            backup_interval_secs: Some(0),
            ..Config::default()
        };
        assert!(interval_secs(&config).is_err());
    }

    #[test]
    fn launchd_plist_caps_logs_via_in_place_truncate_and_exec() {
        let args = RunOnceArgs {
            verify: false,
            ..RunOnceArgs::default()
        };
        let launchd = render(
            Unit::RunOnce,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        let plist = &launchd[0].content;
        // The binary is wrapped so the cap runs before the real process, and
        // `exec` keeps launchd's view of the exit status intact.
        assert!(plist.contains("<string>/bin/sh</string>"));
        assert!(plist.contains("<string>-c</string>"));
        // `exec` keeps launchd's view of the exit status intact (single quotes
        // appear XML-escaped inside the plist string element).
        assert!(plist.contains(
            "exec &apos;/opt/chat-stasher&apos; &apos;run-once&apos; &apos;--stage&apos; &apos;/var/lib/chat-stasher/stage&apos;"
        ));
        // Both logs are capped, using the documented byte cap.
        let cap = format!("{LAUNCHD_LOG_CAP_BYTES}");
        assert!(plist.contains(&format!("-gt {cap} ]")));
        assert!(plist.contains("run-once.log"));
        assert!(plist.contains("run-once.err.log"));
        // Capping must truncate in place — never rotate/rename (which would
        // strand this run's output in the renamed inode).
        assert!(!plist.contains("mv "));
        assert!(!plist.contains(".1\""));
    }

    /// Executes the launchd preamble for real, so it can only run where that
    /// preamble's shell is the one launchd would use. The cap check is
    /// `stat -f%z` — BSD syntax; GNU coreutils spells it `stat -c%s` and fails
    /// the flag outright, so on Linux the guard silently never fires and the
    /// assertion below fails for a reason that has nothing to do with the code
    /// under test. launchd itself is macOS-only, so gating here loses no
    /// coverage: the systemd path has its own tests.
    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_preamble_truncates_only_when_over_cap() {
        let tmp = std::env::temp_dir().join(format!("cs-cap-test-{}", std::process::id()));
        fs::create_dir_all(&tmp).expect("create temp log dir");
        let log = tmp.join("run-once.log");
        fs::write(&log, vec![b'x'; LAUNCHD_LOG_CAP_BYTES + 10]).unwrap();

        let preamble = cap_line(&log, LAUNCHD_LOG_CAP_BYTES);
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("{preamble}\nexec true")])
            .status()
            .expect("run sh preamble");
        assert!(status.success());
        assert_eq!(
            fs::metadata(&log).unwrap().len(),
            0,
            "over-cap log truncated"
        );

        fs::write(&log, vec![b'y'; 64]).unwrap();
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("{preamble}\nexec true")])
            .status()
            .expect("run sh preamble");
        assert!(status.success());
        assert_eq!(
            fs::metadata(&log).unwrap().len(),
            64,
            "under-cap log left intact"
        );

        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sh_single_quote_escapes_embedded_quotes() {
        assert_eq!(sh_single_quote("plain"), "'plain'");
        assert_eq!(sh_single_quote("a'b"), "'a'\"'\"'b'");
        // The hostile classes the shared callers pay it to survive: a value
        // whose space, `$()`, backtick, `;` or newline must never become shell
        // behavior when the rendered line is pasted.
        assert_eq!(
            sh_single_quote("m 3; $(pwned) `bt` 'q' \"d\"\nEnd"),
            "'m 3; $(pwned) `bt` '\"'\"'q'\"'\"' \"d\"\nEnd'"
        );
    }

    #[test]
    fn scheduler_templates_describe_zero_as_the_only_success_status() {
        let args = RunOnceArgs {
            verify: false,
            ..RunOnceArgs::default()
        };
        let files = render(
            Unit::RunOnce,
            Format::Systemd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        assert_eq!(files.len(), 2);
        assert!(files[0].content.contains("SuccessExitStatus=0\n"));
        assert!(!files[0].content.contains("SuccessExitStatus=0 10"));
        assert!(files[0].content.contains("result=NOOP means no snapshot"));
        assert!(files[1].content.contains("OnUnitActiveSec=3600s"));
        let launchd = render(
            Unit::RunOnce,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        assert!(launchd[0]
            .content
            .contains("exit 0 is success; result=NOOP means no snapshot"));
    }

    #[test]
    fn schedule_embeds_destination_in_both_templates() {
        let args = RunOnceArgs {
            destination: Some("external-disk".to_string()),
            verify: true,
            connections: Some(2),
            ..RunOnceArgs::default()
        };
        let launchd = render(
            Unit::RunOnce,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        let plist = &launchd[0].content;
        // The destination must appear as its own argument token, XML-escaped,
        // inside the exec line.
        assert!(plist.contains("exec &apos;/opt/chat-stasher&apos;"));
        assert!(plist.contains("&apos;--destination&apos;"));
        assert!(plist.contains("&apos;external-disk&apos;"));
        assert!(plist.contains("&apos;--verify&apos;"));
        assert!(plist.contains("&apos;--connections&apos;"));
        assert!(plist.contains("&apos;2&apos;"));
        assert!(plist.contains("<string>com.chat-stasher.run-once.external-disk</string>"));

        let systemd = render(
            Unit::RunOnce,
            Format::Systemd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        let service = &systemd[0].content;
        assert!(service.contains("ExecStart="));
        assert!(service.contains("\"--destination\""));
        assert!(service.contains("\"external-disk\""));
        assert!(service.contains("\"--verify\""));
        assert!(service.contains("\"--connections\""));
        assert!(service.contains("\"2\""));
        assert_eq!(
            systemd[0].name,
            "chat-stasher-run-once-external-disk.service"
        );
        assert_eq!(systemd[1].name, "chat-stasher-run-once-external-disk.timer");
    }

    /// A name that is not already filesystem-safe must still yield a stable,
    /// collision-resistant unit name, and the install command must name the
    /// printed files rather than falling back to the unit's default names.
    #[test]
    fn unsafe_destination_names_get_a_stable_suffix_and_install_by_name() {
        let args = RunOnceArgs {
            destination: Some("external disk".to_string()),
            ..RunOnceArgs::default()
        };
        let files = render(
            Unit::RunOnce,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        assert_eq!(files.len(), 1);
        let label = files[0].name.trim_end_matches(".plist");
        assert!(label.starts_with("com.chat-stasher.run-once.external-disk-"));
        assert!(
            files[0]
                .content
                .contains(&format!("<string>{label}</string>")),
            "the plist Label must match its file name"
        );

        let command = install_command_for_files(Format::Launchd, &files);
        assert!(command.contains(&files[0].name));
    }

    #[test]
    fn systemd_install_command_uses_destination_unit_names() {
        let args = RunOnceArgs {
            destination: Some("external-disk".to_string()),
            ..RunOnceArgs::default()
        };
        let files = render(
            Unit::RunOnce,
            Format::Systemd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        let paths = files
            .iter()
            .map(|file| PathBuf::from("/tmp/templates").join(&file.name))
            .collect::<Vec<_>>();
        let command = install_command(Unit::RunOnce, Format::Systemd, &paths);
        assert!(command
            .contains("$HOME/.config/systemd/user/chat-stasher-run-once-external-disk.service"));
        assert!(command
            .contains("$HOME/.config/systemd/user/chat-stasher-run-once-external-disk.timer"));
    }

    /// A multi-destination render produces one service+timer pair per
    /// destination. The printed install command must install every pair and
    /// enable every timer, not only the first — an earlier version found the
    /// first `.service` / `.timer` and silently dropped the rest.
    #[test]
    fn systemd_install_command_covers_every_destination() {
        let mut files = Vec::new();
        for destination in ["alpha", "beta"] {
            let args = RunOnceArgs {
                destination: Some(destination.to_string()),
                ..RunOnceArgs::default()
            };
            files.extend(render(
                Unit::RunOnce,
                Format::Systemd,
                Path::new("/opt/chat-stasher"),
                Path::new("/var/lib/chat-stasher/stage"),
                3600,
                &args,
                &ReclaimStageArgs::default(),
                Path::new("/home/tester"),
            ));
        }
        let paths = files
            .iter()
            .map(|file| PathBuf::from("/tmp/templates").join(&file.name))
            .collect::<Vec<_>>();
        let command = install_command(Unit::RunOnce, Format::Systemd, &paths);
        for name in [
            "chat-stasher-run-once-alpha.service",
            "chat-stasher-run-once-alpha.timer",
            "chat-stasher-run-once-beta.service",
            "chat-stasher-run-once-beta.timer",
        ] {
            assert!(
                command.contains(&format!("\"$HOME/.config/systemd/user/{name}\"")),
                "install command must install {name}: {command}"
            );
        }
        assert!(command.contains("systemctl --user enable --now chat-stasher-run-once-alpha.timer"));
        assert!(command.contains("systemctl --user enable --now chat-stasher-run-once-beta.timer"));
    }

    /// launchd has no random-delay key for `StartInterval`, so the shell
    /// preamble must sleep for a bounded random interval *before* `exec`.
    #[test]
    fn launchd_preamble_bounds_jitter_before_exec() {
        let args = RunOnceArgs::default();
        let files = render(
            Unit::RunOnce,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        let plist = &files[0].content;
        let modulus = SCHEDULER_RANDOMIZED_DELAY_SECS + 1;
        assert!(plist.contains(&format!(
            "jitter=$(( $(od -An -N2 -tu2 /dev/urandom) % {modulus} ))"
        )));
        let sleep = plist
            .find("sleep &quot;$jitter&quot;")
            .expect("jitter sleep");
        let exec = plist.find("exec &apos;").expect("exec");
        assert!(sleep < exec, "jitter must be applied before exec");
    }

    /// The weekly unit must embed the `reclaim-stage` subcommand, the stage path
    /// and `--apply` in *both* formats. This is the regression guard for the
    /// class of bug that once shipped `schedule` without `--destination`:
    /// string-containment alone missed it, so this test also checks the
    /// machine-facing shape of each renderer (calendar keys for launchd,
    /// unit names + OnCalendar for systemd).
    #[test]
    fn reclaim_stage_templates_embed_stage_and_subcommand_in_both_formats() {
        let reclaim_args = ReclaimStageArgs {
            repo: Some("backup-repo".to_string()),
            connections: Some(3),
            ..ReclaimStageArgs::default()
        };

        let launchd = render(
            Unit::ReclaimStage,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            0,
            &RunOnceArgs::default(),
            &reclaim_args,
            Path::new("/home/tester"),
        );
        assert_eq!(launchd.len(), 1);
        assert_eq!(launchd[0].name, "com.chat-stasher.reclaim-stage.plist");
        let plist = &launchd[0].content;
        assert!(plist.contains("&apos;reclaim-stage&apos;"));
        assert!(plist.contains("&apos;/var/lib/chat-stasher/stage&apos;"));
        assert!(plist.contains("&apos;--apply&apos;"));
        assert!(plist.contains("&apos;--repo&apos;"));
        assert!(plist.contains("&apos;backup-repo&apos;"));
        // Logs sit next to the run-once logs but in separate files.
        assert!(plist.contains("reclaim-stage.log"));
        assert!(plist.contains("reclaim-stage.err.log"));
        assert!(!plist.contains("run-once.log"));
        // The weekly slot is a fixed calendar interval, never StartInterval.
        assert!(plist.contains("<key>StartCalendarInterval</key>"));
        assert!(!plist.contains("<key>StartInterval</key>"));
        assert!(plist.contains("<integer>0</integer>")); // Weekday = Sunday
        assert!(plist.contains("<integer>3</integer>")); // Hour
        assert!(plist.contains("<integer>17</integer>")); // Minute

        let systemd = render(
            Unit::ReclaimStage,
            Format::Systemd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            0,
            &RunOnceArgs::default(),
            &reclaim_args,
            Path::new("/home/tester"),
        );
        assert_eq!(systemd.len(), 2);
        assert_eq!(systemd[0].name, "chat-stasher-reclaim-stage.service");
        assert_eq!(systemd[1].name, "chat-stasher-reclaim-stage.timer");
        let service = &systemd[0].content;
        assert!(service.contains("ExecStart="));
        assert!(service.contains("\"reclaim-stage\""));
        assert!(service.contains("\"/var/lib/chat-stasher/stage\""));
        assert!(service.contains("\"--apply\""));
        assert!(service.contains("\"--repo\""));
        assert!(service.contains("\"backup-repo\""));
        let timer = &systemd[1].content;
        assert!(timer.contains("OnCalendar=Sun *-*-* 03:17:00"));
        assert!(timer.contains("RandomizedDelaySec=900s"));
        assert!(timer.contains("Unit=chat-stasher-reclaim-stage.service"));
        assert!(!timer.contains("chat-stasher-run-once.timer"));
    }

    #[test]
    fn reclaim_stage_unit_names_do_not_collide_with_run_once() {
        assert_ne!(
            launchd_label(Unit::RunOnce),
            launchd_label(Unit::ReclaimStage)
        );
        assert_ne!(
            systemd_service_name(Unit::RunOnce),
            systemd_service_name(Unit::ReclaimStage)
        );
        assert_ne!(
            systemd_timer_name(Unit::RunOnce),
            systemd_timer_name(Unit::ReclaimStage)
        );
        // The install commands must target the unit they belong to.
        let run = install_command_for_files(
            Format::Systemd,
            &[TemplateFile {
                name: SYSTEMD_TIMER.to_string(),
                content: String::new(),
            }],
        );
        let reclaim = install_command_for_files(
            Format::Systemd,
            &[TemplateFile {
                name: SYSTEMD_TIMER_RECLAIM_STAGE.to_string(),
                content: String::new(),
            }],
        );
        assert!(run.contains("chat-stasher-run-once.timer"));
        assert!(!run.contains("chat-stasher-reclaim-stage.timer"));
        assert!(reclaim.contains("chat-stasher-reclaim-stage.timer"));
        assert!(!reclaim.contains("chat-stasher-run-once.timer"));
    }

    #[test]
    fn binary_resolution_prefers_installed_copy_over_cargo_artifact() {
        let temp = tempfile::tempdir().expect("create test directory");
        let home = temp.path().join("home");
        let installed = home.join(".local/bin/chat-stasher");
        fs::create_dir_all(installed.parent().unwrap()).expect("create local bin");
        fs::write(&installed, b"binary").expect("write installed marker");

        let cargo_binary = temp.path().join("target/release/chat-stasher");
        assert_eq!(
            resolve_binary(None, &cargo_binary, &home).unwrap(),
            installed
        );
        assert!(resolve_binary(Some(&cargo_binary), &cargo_binary, &home).is_err());

        let installed_current = temp.path().join("bin/chat-stasher");
        assert_eq!(
            resolve_binary(None, &installed_current, &home).unwrap(),
            installed_current
        );
    }

    #[cfg(unix)]
    #[test]
    fn launchd_install_and_uninstall_are_idempotent_with_fake_launchctl() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("launchctl");
        let state = temp.path().join("loaded");
        let log = temp.path().join("calls");
        let script_body = format!(
            "#!/bin/sh\n\
             echo \"$@\" >> \"{}\"\n\
             case \"$1\" in\n\
               print) test -f \"{}\";;\n\
               bootstrap) touch \"{}\";;\n\
               bootout) rm -f \"{}\";;\n\
               *) exit 2;;\n\
             esac\n",
            log.display(),
            state.display(),
            state.display(),
            state.display()
        );
        fs::write(&script, script_body).expect("write fake launchctl");
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("make fake launchctl executable");

        let file = TemplateFile {
            name: "com.chat-stasher.run-once.disk.plist".to_string(),
            content: "plist-v1".to_string(),
        };
        assert_eq!(
            install_launchd_agents(temp.path(), &[file.clone()], &script, "gui/test").unwrap(),
            vec![InstallResult::Installed]
        );
        assert_eq!(
            install_launchd_agents(temp.path(), &[file], &script, "gui/test").unwrap(),
            vec![InstallResult::Unchanged]
        );
        assert_eq!(
            uninstall_launchd_agents(
                temp.path(),
                &["com.chat-stasher.run-once.disk".to_string()],
                &script,
                "gui/test"
            )
            .unwrap(),
            1
        );
        assert_eq!(
            uninstall_launchd_agents(
                temp.path(),
                &["com.chat-stasher.run-once.disk".to_string()],
                &script,
                "gui/test"
            )
            .unwrap(),
            0
        );
        assert!(!temp
            .path()
            .join("Library/LaunchAgents/com.chat-stasher.run-once.disk.plist")
            .exists());
        let calls = fs::read_to_string(log).expect("read fake launchctl log");
        assert_eq!(calls.matches("bootstrap").count(), 1);
        assert_eq!(calls.matches("bootout").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn systemd_install_and_uninstall_are_idempotent_with_fake_systemctl() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("systemctl");
        let state = temp.path().join("active");
        let log = temp.path().join("calls");
        let script_body = format!(
            "#!/bin/sh\n\
             echo \"$@\" >> \"{}\"\n\
             case \"$2\" in\n\
               is-active) test -f \"{}\";;\n\
               enable) touch \"{}\";;\n\
               disable) rm -f \"{}\";;\n\
               *) exit 0;;\n\
             esac\n",
            log.display(),
            state.display(),
            state.display(),
            state.display()
        );
        fs::write(&script, script_body).expect("write fake systemctl");
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("make fake systemctl executable");

        let files = vec![
            TemplateFile {
                name: "chat-stasher-run-once.service".into(),
                content: "service-v1".into(),
            },
            TemplateFile {
                name: "chat-stasher-run-once.timer".into(),
                content: "timer-v1".into(),
            },
        ];
        assert_eq!(
            install_systemd_units(temp.path(), &files, &script).unwrap(),
            vec![InstallResult::Installed]
        );
        assert_eq!(
            install_systemd_units(temp.path(), &files, &script).unwrap(),
            vec![InstallResult::Unchanged]
        );
        let timer = "chat-stasher-run-once.timer".to_string();
        assert_eq!(
            uninstall_systemd_units(temp.path(), std::slice::from_ref(&timer), &script).unwrap(),
            2
        );
        assert_eq!(
            uninstall_systemd_units(temp.path(), &[timer], &script).unwrap(),
            0
        );
        let calls = fs::read_to_string(log).expect("read fake systemctl log");
        assert_eq!(calls.matches("enable --now").count(), 1);
        assert_eq!(calls.matches("disable --now").count(), 1);
    }

    /// The plists `render` produced, saved where the installer saves them, so a
    /// probe reads the same bytes launchd would.
    fn write_plists(home: &Path, files: &[TemplateFile]) {
        let agents = home.join("Library/LaunchAgents");
        fs::create_dir_all(&agents).expect("create launchd agent directory");
        for file in files {
            fs::write(agents.join(&file.name), &file.content).expect("write plist");
        }
    }

    fn local_noon() -> DateTime<Local> {
        // 2026-09-23 is a Wednesday, so "the next Sunday" is a real step and
        // not the day the test happens to run on.
        Local
            .with_ymd_and_hms(2026, 9, 23, 12, 0, 0)
            .earliest()
            .expect("2026-09-23 12:00 exists in local time")
    }

    /// A calendar plist is its own source: the fire time is computed from the
    /// `StartCalendarInterval` keys the plist carries, on the local calendar.
    #[test]
    fn launchd_calendar_plist_yields_the_exact_next_local_occurrence() {
        use chrono::Timelike;

        let home = tempfile::tempdir().unwrap();
        let files = render(
            Unit::ReclaimStage,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            0,
            &RunOnceArgs::default(),
            &ReclaimStageArgs::default(),
            home.path(),
        );
        write_plists(home.path(), &files);

        let now = local_noon();
        let next = next_run(
            Unit::ReclaimStage,
            Format::Launchd,
            &[None],
            home.path(),
            Path::new("launchctl"),
            now,
        );
        let value = next.value().expect("the plist names a calendar slot");
        assert_eq!(next.note(), None, "a known time carries no excuse");

        let when = DateTime::parse_from_str(value, "%a %Y-%m-%d %H:%M:%S %z")
            .expect("the reported value is a local timestamp");
        assert_eq!(
            when.date_naive(),
            NaiveDate::from_ymd_opt(2026, 9, 27).unwrap()
        );
        assert_eq!(when.weekday(), chrono::Weekday::Sun);
        // The slot the plist declares is Sunday 03:17 — spelled out rather
        // than read back off the constants it was built from.
        assert_eq!((when.hour(), when.minute()), (3, 17));
        assert!(when > now, "the next occurrence is never in the past");
    }

    /// The same slot, asked again after it has passed, is the following week's.
    #[test]
    fn a_calendar_slot_that_has_already_passed_moves_to_next_week() {
        let home = tempfile::tempdir().unwrap();
        let files = render(
            Unit::ReclaimStage,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            0,
            &RunOnceArgs::default(),
            &ReclaimStageArgs::default(),
            home.path(),
        );
        write_plists(home.path(), &files);

        // Sunday 04:00, an hour after the slot has fired.
        let now = Local
            .with_ymd_and_hms(2026, 9, 27, 4, 0, 0)
            .earliest()
            .expect("2026-09-27 04:00 exists in local time");
        let next = next_run(
            Unit::ReclaimStage,
            Format::Launchd,
            &[None],
            home.path(),
            Path::new("launchctl"),
            now,
        );
        let when = DateTime::parse_from_str(
            next.value().expect("a calendar slot is known"),
            "%a %Y-%m-%d %H:%M:%S %z",
        )
        .expect("the reported value is a local timestamp");
        assert_eq!(
            when.date_naive(),
            NaiveDate::from_ymd_opt(2026, 10, 4).unwrap()
        );
    }

    /// launchd reports no fire time for an interval job, so the answer is the
    /// sentence saying why — with the interval the plist declares.
    #[test]
    fn launchd_interval_plist_says_why_no_fire_time_is_reported() {
        let home = tempfile::tempdir().unwrap();
        let files = render(
            Unit::RunOnce,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &RunOnceArgs::default(),
            &ReclaimStageArgs::default(),
            home.path(),
        );
        write_plists(home.path(), &files);

        let next = next_run(
            Unit::RunOnce,
            Format::Launchd,
            &[None],
            home.path(),
            Path::new("launchctl"),
            local_noon(),
        );
        assert_eq!(next.value(), None);
        assert_eq!(
            next.note(),
            Some(
                "launchd interval jobs expose no next fire time; the job runs every 60 minutes \
                 after load"
            )
        );
    }

    /// A plist that cannot be read is not a plist that declares nothing: the
    /// two must not collapse into one answer.
    #[test]
    fn an_unreadable_launchd_plist_is_reported_as_unread_not_as_absent() {
        let home = tempfile::tempdir().unwrap();
        let next = next_run(
            Unit::RunOnce,
            Format::Launchd,
            &[None],
            home.path(),
            Path::new("launchctl"),
            local_noon(),
        );
        assert_eq!(next.value(), None);
        let note = next.note().expect("an empty answer carries its reason");
        assert!(note.contains("could not be read"), "note={note}");
        assert!(
            note.contains("com.chat-stasher.run-once.plist"),
            "the unreadable file is named: note={note}"
        );
    }

    /// Only the `Trigger:` line is a fire time. `n/a` is systemd's own spelling
    /// for "no next elapse" and must not be read as one.
    #[test]
    fn the_systemd_answer_comes_from_the_trigger_line_alone() {
        let status = "● chat-stasher-run-once.timer - Hourly chat-stasher archive cycle\n     \
                      Loaded: loaded (/home/u/.config/systemd/user/chat-stasher-run-once.timer)\n     \
                      Active: active (waiting) since Wed 2026-09-23 12:00:00 CEST; 2h ago\n    \
                      Trigger: Sun 2026-09-27 03:17:00 CEST; 3 days left\n   \
                      Triggers: ● chat-stasher-run-once.service\n";
        assert_eq!(
            systemd_trigger_line(status).as_deref(),
            Some("Sun 2026-09-27 03:17:00 CEST")
        );
        assert_eq!(systemd_trigger_line("    Trigger: n/a\n"), None);
        // "TriggeredBy:" begins with the same eight characters, and a unit line
        // is not a timestamp.
        assert_eq!(
            systemd_trigger_line("   TriggeredBy: ● chat-stasher-run-once.timer\n"),
            None
        );
        assert_eq!(systemd_trigger_line(""), None);
    }

    /// One next run cannot stand for several timers, and an absent unit list is
    /// not a timer that is merely unknown.
    #[test]
    fn one_next_run_cannot_stand_for_several_timers() {
        let home = tempfile::tempdir().unwrap();
        let several = next_run(
            Unit::RunOnce,
            Format::Systemd,
            &[Some("a".to_string()), Some("b".to_string())],
            home.path(),
            Path::new("systemctl"),
            local_noon(),
        );
        assert_eq!(several.value(), None);
        let note = several.note().expect("the empty answer carries its reason");
        assert!(
            note.contains("2 destinations have one timer each"),
            "note={note}"
        );

        let none = next_run(
            Unit::RunOnce,
            Format::Systemd,
            &[],
            home.path(),
            Path::new("systemctl"),
            local_noon(),
        );
        assert_eq!(none.value(), None);
        assert!(none
            .note()
            .expect("the empty answer carries its reason")
            .contains("no scheduler unit was installed"));
    }

    /// The three things a systemd probe can get back, each with its own state:
    /// systemd's own timestamp, its `n/a`, and a `systemctl` that cannot be run
    /// at all. A fake stands in for the scheduler so each one is reachable on
    /// any host.
    #[cfg(unix)]
    #[test]
    fn the_systemd_probe_reports_only_what_systemctl_answered() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("systemctl");
        let answer = temp.path().join("answer");
        fs::write(
            &answer,
            "    Trigger: Sun 2026-09-27 03:17:00 CEST; 3 days left\n",
        )
        .expect("write answer");
        // `cat` reproduces the answer file, so the branch under test is the
        // parse and not the fixture.
        fs::write(
            &script,
            format!("#!/bin/sh\ncat \"{}\"\n", answer.display()),
        )
        .expect("write fake systemctl");
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("make fake systemctl executable");

        let now = local_noon();
        let known = next_run(
            Unit::RunOnce,
            Format::Systemd,
            &[None],
            temp.path(),
            &script,
            now,
        );
        assert_eq!(known.value(), Some("Sun 2026-09-27 03:17:00 CEST"));
        assert_eq!(known.note(), None);

        fs::write(&answer, "    Trigger: n/a\n").expect("write n/a answer");
        let unarmed = next_run(
            Unit::RunOnce,
            Format::Systemd,
            &[None],
            temp.path(),
            &script,
            now,
        );
        assert_eq!(unarmed.value(), None);
        assert!(
            unarmed
                .note()
                .expect("the empty answer carries its reason")
                .contains("did not report a next run"),
            "note={:?}",
            unarmed.note()
        );

        let missing = next_run(
            Unit::RunOnce,
            Format::Systemd,
            &[None],
            temp.path(),
            &temp.path().join("no-such-systemctl"),
            now,
        );
        assert_eq!(missing.value(), None);
        assert!(
            missing
                .note()
                .expect("the empty answer carries its reason")
                .contains("could not be run"),
            "note={:?}",
            missing.note()
        );
    }

    /// The installed shape is one unit per named destination, or one per
    /// declared destination when none were named — the same set the installer
    /// writes units for.
    #[test]
    fn install_targets_follow_the_named_destinations_or_all_declared_ones() {
        let declared = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            install_targets(&declared, &[]),
            vec![Some("a".to_string()), Some("b".to_string())]
        );
        assert_eq!(
            install_targets(&declared, &["b".to_string()]),
            vec![Some("b".to_string())]
        );
        assert_eq!(install_targets(&[], &[]), vec![None]);
    }
}
