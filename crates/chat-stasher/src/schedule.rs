//! Render scheduler templates without installing or registering them.
//!
//! The scheduler is deliberately external to chat-stasher: launchd/systemd
//! starts one run-once process, which exits after the pass. Rendering remains
//! side-effect free; the launchd install helpers are explicit and testable.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Days, Local, LocalResult, NaiveDate, TimeZone, Timelike};
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
/// ([`RECLAIM_STAGE_RANDOMIZED_DELAY_SECS`]). The hourly run-once is a
/// `StartCalendarInterval` too, at the minute [`hourly_minute_for_machine`]
/// picks for the machine it is installed on, so a machine that picked `:17`
/// fires both jobs on the same minute — but a weekly ~20 minute network-bound
/// pass occasionally overlapping the hourly pass is a non-event compared with
/// running it on top of interactive work.
pub const RECLAIM_STAGE_WEEKDAY: u8 = 0; // launchd: 0 and 7 both mean Sunday.
pub const RECLAIM_STAGE_HOUR: u8 = 3;
pub const RECLAIM_STAGE_MINUTE: u8 = 17;

/// systemd `RandomizedDelaySec` for the weekly timer, in seconds: up to 15
/// minutes of random start delay so a fleet of machines does not all hit their
/// destination at 03:17:00 on the same second.
pub const RECLAIM_STAGE_RANDOMIZED_DELAY_SECS: u64 = 15 * 60;

/// Per-run scheduler jitter, in seconds. launchd has no random-delay key for
/// `StartInterval` or `StartCalendarInterval`, so the shell preamble sleeps for
/// a bounded random interval before `exec`; systemd gets the same bound through
/// `RandomizedDelaySec`.
pub const SCHEDULER_RANDOMIZED_DELAY_SECS: u64 = 5 * 60;

/// The cadence, in seconds, at which the launchd `run-once` timer is
/// rendered as a `StartCalendarInterval` (one fire per hour at a fixed
/// minute) rather than a `StartInterval`. Exactly one hour is the only
/// interval whose calendar form is a single `Minute` key; every other
/// interval keeps `StartInterval`.
const HOURLY_INTERVAL_SECS: u64 = 3600;

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

/// Scheduler identity for a named inbox. Its namespace is independent of
/// archive and reclamation jobs, including when their names are identical.
pub fn pull_label(name: &str) -> String {
    format!(
        "com.chat-stasher.inbox-pull.{}",
        destination_component(name)
    )
}

pub fn pull_timer(name: &str) -> String {
    format!(
        "chat-stasher-inbox-pull-{}.timer",
        destination_component(name)
    )
}

/// Render only the trusted pull command. No archive credentials, collection,
/// push or reclamation slots belong to this execution path.
#[allow(clippy::too_many_arguments)]
pub fn render_pull(
    format: Format,
    binary: &Path,
    stage: &Path,
    interval: u64,
    name: &str,
    machine: Option<&str>,
    shard_bucket_cap: Option<usize>,
    home: &Path,
) -> Vec<TemplateFile> {
    let mut argv = vec![
        binary.display().to_string(),
        "inbox-pull".into(),
        name.into(),
        "--stage".into(),
        stage.display().to_string(),
    ];
    if let Some(machine) = machine {
        argv.extend(["--machine".into(), machine.into()]);
    }
    if let Some(cap) = shard_bucket_cap {
        argv.extend(["--shard-bucket-cap".into(), cap.to_string()]);
    }
    match format {
        Format::Launchd => {
            let label = pull_label(name);
            vec![TemplateFile {
                name: format!("{label}.plist"),
                content: render_launchd_job(&argv, interval, home, &label, &label),
            }]
        }
        Format::Systemd => {
            let timer = pull_timer(name);
            let service = timer.replace(".timer", ".service");
            vec![
                TemplateFile {
                    name: service.clone(),
                    content: render_systemd_job(&argv, &service, "inbox pull"),
                },
                TemplateFile {
                    name: timer,
                    content: render_systemd_interval_timer(
                        interval,
                        &service,
                        "Periodic chat-stasher inbox pull",
                    ),
                },
            ]
        }
    }
}

/// Resolve the executable that a persistent scheduler may safely embed.
/// Cargo's `target/` paths and `CARGO_TARGET_DIR` are disposable build products.
/// An explicit installed path need not exist yet, so a package manager can
/// render before its copy is complete.
pub fn resolve_binary(explicit: Option<&Path>, current_exe: &Path, home: &Path) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = absolute_path(path);
        if is_build_artifact(&path) {
            bail!(
                "binary path must be an installed path outside Cargo build directories: {}",
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
    let cargo_target = std::env::var_os("CARGO_TARGET_DIR");
    is_build_artifact_in_target(path, cargo_target.as_deref().map(Path::new))
}

fn is_build_artifact_in_target(path: &Path, cargo_target: Option<&Path>) -> bool {
    if let Some(target) = cargo_target.filter(|target| !target.as_os_str().is_empty()) {
        if path.starts_with(absolute_path(target)) {
            return true;
        }
    }
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

/// One agent file [`install_launchd_agents`] changed, and everything needed to
/// put it back: the bytes it had before the call (`None` = the file did not
/// exist, so the roll-back deletes it), and whether launchd had that agent
/// loaded at that moment.
///
/// `was_loaded` is captured before the first change, which is the only moment
/// the answer describes the pre-call machine: once `bootout` has run, "is it
/// loaded?" answers about the failed install rather than about what it found.
struct LaunchdChange {
    path: PathBuf,
    label: String,
    previous: Option<Vec<u8>>,
    was_loaded: bool,
}

/// Install (or re-install) a rendered set of launchd user agents.
///
/// The ordering rule is the same one [`install_systemd_units`] follows, and it
/// exists for the same reason: `launchctl bootstrap` cannot load a plist that
/// is not on disk yet, so the step that can refuse runs *after* the file is
/// written. A refusal therefore happens on a machine whose disk already holds
/// part of a schedule launchd will not load — the state W282 §2 caught `status`
/// reporting as `installed: true`. So this call rolls itself back instead: on
/// any failure after the first write it unloads the agents it got loaded,
/// removes the plists it created and restores the ones it replaced, then loads
/// again the agents that were loaded before it ran.
///
/// Within one file the new bytes are written *before* the running agent is
/// booted out, so a write that fails leaves the loaded job and the file it
/// reads describing the same schedule, and the roll-back has no agent to
/// re-load. A failed *re*install is the case this ordering protects: the
/// working agent stays up until its replacement is on disk.
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
    let mut changed: Vec<LaunchdChange> = Vec::new();
    for file in files {
        let path = agents.join(&file.name);
        let label = file
            .name
            .strip_suffix(".plist")
            .unwrap_or(&file.name)
            .to_string();
        let target = format!("{domain}/{label}");
        let previous = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                // Refuse to overwrite a plist this call could not read back,
                // for the reason the systemd arm gives: the roll-back restores
                // what it saved, and a file that cannot be read cannot be
                // saved. Without this refusal a later failure in the same call
                // would delete an agent it never understood.
                bail!(
                    "launchd agent {} exists but cannot be read ({error}), so a failed \
                     install could not put it back the way it was; make the file readable \
                     and run the install again",
                    path.display()
                );
            }
        };
        let same = previous.as_deref() == Some(file.content.as_bytes()); // reason: an absent agent file is treated as changed, so install rewrites and reloads it
        let loaded = match launchctl_status(launchctl, &target) {
            Ok(loaded) => loaded,
            // Nothing has been touched yet for this file, but earlier files in
            // this call may have been, so the roll-back still has work to do.
            Err(error) => return Err(rollback_launchd_install(error, &changed, launchctl, domain)),
        };
        if same && loaded {
            results.push(InstallResult::Unchanged);
            continue;
        }
        // Past this point the call is committed to changing this agent, so
        // record what it looked like first.
        changed.push(LaunchdChange {
            path: path.clone(),
            label: label.clone(),
            previous,
            was_loaded: loaded,
        });
        if !same {
            if let Err(error) = write_atomic(&path, file.content.as_bytes()) {
                let error = anyhow::Error::from(error)
                    .context(format!("write launchd agent {}", path.display()));
                return Err(rollback_launchd_install(error, &changed, launchctl, domain));
            }
        }
        if loaded {
            if let Err(error) = launchctl_run(launchctl, &["bootout", domain, &label]) {
                let error = error.context(format!("unload launchd agent {label}"));
                return Err(rollback_launchd_install(error, &changed, launchctl, domain));
            }
        }
        if let Err(error) =
            launchctl_run(launchctl, &["bootstrap", domain, &path.to_string_lossy()])
        {
            let error = error.context(format!("load launchd agent {label}"));
            return Err(rollback_launchd_install(error, &changed, launchctl, domain));
        }
        results.push(InstallResult::Installed);
    }
    Ok(results)
}

/// Undo what a failed [`install_launchd_agents`] did to the machine, and attach
/// a sentence about the outcome to the error that stopped the install.
///
/// The systemd roll-back's contract, in launchd's verbs. Every step is
/// best-effort by construction: the caller's error happened first and must stay
/// the primary error, so a roll-back step that fails is reported in the context
/// rather than replacing what it failed to undo.
///
/// Each agent is unloaded before its file is restored, so launchd is never left
/// holding a loaded job whose definition is about to change underneath it; and
/// an agent that was loaded before this call is loaded again afterwards,
/// because unloading it is part of what the failed install did.
fn rollback_launchd_install(
    error: anyhow::Error,
    changed: &[LaunchdChange],
    launchctl: &Path,
    domain: &str,
) -> anyhow::Error {
    let mut unrestored: Vec<String> = Vec::new();
    for entry in changed {
        match launchctl_status(launchctl, &format!("{domain}/{}", entry.label)) {
            Ok(true) => {
                if let Err(stop_error) =
                    launchctl_run(launchctl, &["bootout", domain, &entry.label])
                {
                    unrestored.push(format!(
                        "the agent {} stayed loaded ({stop_error}) — a schedule this failed \
                         install had already loaded is still loaded",
                        entry.label
                    ));
                }
            }
            Ok(false) => {}
            Err(ask_error) => unrestored.push(format!(
                "launchctl could not be asked whether {} was loaded ({ask_error}), so it may \
                 still be loaded",
                entry.label
            )),
        }
        let undone = match &entry.previous {
            Some(bytes) => fs::write(&entry.path, bytes)
                .with_context(|| format!("restore {}", entry.path.display())),
            None => fs::remove_file(&entry.path)
                .with_context(|| format!("remove {}", entry.path.display())),
        };
        if let Err(undo_error) = undone {
            unrestored.push(format!("{undo_error:#}"));
        }
        if entry.was_loaded {
            let restored = entry.path.to_string_lossy().into_owned();
            if let Err(load_error) = launchctl_run(launchctl, &["bootstrap", domain, &restored]) {
                unrestored.push(format!(
                    "the agent {} was loaded before this install and could not be loaded \
                     again ({load_error})",
                    entry.label
                ));
            }
        }
    }
    if unrestored.is_empty() {
        error.context(
            "the failed install was rolled back: every agent it created was removed, every \
             one it replaced got its previous content back and every agent that was loaded \
             before it is loaded again, so the machine is as it was before `schedule \
             install` ran",
        )
    } else {
        error.context(format!(
            "the failed install was rolled back incompletely — part of it is still on the \
             machine: {}. Run `schedule uninstall` again once launchctl is usable, and \
             check `launchctl list` before trusting any schedule it reports",
            unrestored.join("; ")
        ))
    }
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

/// Install (or re-install) a rendered set of systemd user units.
///
/// The scheduler steps that can refuse — `daemon-reload`, enabling each timer —
/// all run *after* the unit files have been written, because `daemon-reload`
/// cannot read a unit that is not on disk yet. A refusal from any of them
/// therefore happens on a machine whose disk already holds part of a schedule
/// no manager will arm, which is precisely the state W282 §2 caught `status`
/// reporting as `installed: true`: file presence alone was the probe, and the
/// files were present. So this call rolls itself back instead: on any failure
/// after writing starts it stops the timers it enabled, restores the bytes
/// each unit file had before this call (or removes the files it created), and
/// asks a manager that had re-read to re-read again, so the machine ends the
/// failed install the way it began it.
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
    // What each unit file this call rewrote had under it before: `None` means
    // the file did not exist, so the roll-back deletes it rather than writing
    // something back. The bytes are captured *before* the first write, which
    // is the only moment they can be captured.
    let mut written: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    for file in files {
        let path = units.join(&file.name);
        let previous = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                // Refuse to overwrite a file this call could not read back.
                // The roll-back below restores what it saved, and a file that
                // cannot be read cannot be saved; without this refusal an
                // install that later failed would delete a unit it never
                // understood, which is not the pre-call state either.
                bail!(
                    "systemd unit {} exists but cannot be read ({error}), so a failed \
                     install could not put it back the way it was; make the file readable \
                     and run the install again",
                    path.display()
                );
            }
        };
        let same = previous.as_deref() == Some(file.content.as_bytes()); // reason: an absent or unreadable unit is treated as changed, so install rewrites and reloads it
        if !same {
            if let Err(error) = write_atomic(&path, file.content.as_bytes()) {
                let error = anyhow::Error::from(error)
                    .context(format!("write systemd unit {}", path.display()));
                return Err(rollback_systemd_install(
                    error,
                    &written,
                    &[],
                    false,
                    systemctl,
                ));
            }
            written.push((path, previous));
            changed = true;
        }
        if file.name.ends_with(".timer") {
            timers.push(file.name.clone());
        }
    }
    if let Err(error) = systemctl_run(systemctl, &["--user", "daemon-reload"]) {
        // The manager has re-read nothing, so restoring the disk is the whole
        // undo: any in-memory units still describe the restored bytes.
        return Err(rollback_systemd_install(
            error,
            &written,
            &[],
            false,
            systemctl,
        ));
    }
    let mut active = true;
    for timer in &timers {
        let output = Command::new(systemctl)
            .args(["--user", "is-active", "--quiet", timer])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("check systemd timer {timer}"));
        match output {
            Ok(output) => active &= output.success(),
            Err(error) => {
                return Err(rollback_systemd_install(
                    error,
                    &written,
                    &[],
                    true,
                    systemctl,
                ));
            }
        }
    }
    let mut enabled: Vec<String> = Vec::new();
    if !active || changed {
        for timer in &timers {
            if let Err(error) = systemctl_run(systemctl, &["--user", "enable", "--now", timer]) {
                return Err(rollback_systemd_install(
                    error, &written, &enabled, true, systemctl,
                ));
            }
            enabled.push(timer.clone());
        }
    }
    Ok(vec![if changed || !active {
        InstallResult::Installed
    } else {
        InstallResult::Unchanged
    }])
}

/// Undo what a failed [`install_systemd_units`] did to the machine, and attach
/// a sentence about the outcome to the error that stopped the install.
///
/// Every step is best-effort by construction: the caller's error happened
/// first and must stay the primary error, so a roll-back step that fails is
/// reported in the context rather than replacing what it failed to undo.
///
/// `reload_happened` says whether `daemon-reload` had succeeded before the
/// failure: only then did the manager re-read the units this call had already
/// rewritten, so only then does the roll-back have to ask it to re-read the
/// restored bytes — otherwise the manager's in-memory units still describe
/// the pre-call state the disk now has again.
fn rollback_systemd_install(
    error: anyhow::Error,
    written: &[(PathBuf, Option<Vec<u8>>)],
    enabled: &[String],
    reload_happened: bool,
    systemctl: &Path,
) -> anyhow::Error {
    let mut unrestored: Vec<String> = Vec::new();
    for timer in enabled {
        if let Err(stop_error) = systemctl_run(systemctl, &["--user", "disable", "--now", timer]) {
            unrestored.push(format!(
                "the timer {timer} stayed armed ({stop_error}) — a schedule this failed \
                 install had already enabled is still enabled"
            ));
        }
    }
    for (path, previous) in written {
        let undone = match previous {
            Some(bytes) => {
                fs::write(path, bytes).with_context(|| format!("restore {}", path.display()))
            }
            None => fs::remove_file(path).with_context(|| format!("remove {}", path.display())),
        };
        if let Err(undo_error) = undone {
            unrestored.push(format!("{undo_error:#}"));
        }
    }
    if reload_happened {
        if let Err(reload_error) = systemctl_run(systemctl, &["--user", "daemon-reload"]) {
            unrestored.push(format!(
                "daemon-reload could not run after the roll-back ({reload_error}), so the \
                 manager may still hold unit definitions the files on disk no longer contain"
            ));
        }
    }
    if unrestored.is_empty() {
        error.context(
            "the failed install was rolled back: every unit file it created was removed and \
             every one it replaced got its previous content back, so the machine is as it was \
             before `schedule install` ran",
        )
    } else {
        error.context(format!(
            "the failed install was rolled back incompletely — part of it is still on the \
             machine: {}. Run `schedule uninstall` again once the manager is usable, and \
             check `systemctl --user list-timers` before trusting any schedule it reports",
            unrestored.join("; ")
        ))
    }
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

/// The state of one scheduled job on this machine, from the install `schedule
/// install` performs.
///
/// The two formats answer differently, and the difference is deliberate:
///
/// * **systemd** is asked, not just looked at. W282 §2 measured the failure
///   mode of asking less: with no user session the install's `daemon-reload`
///   failed *after* the unit files were on disk, no timer was armed, and
///   `status` — whose one job is answering "is the scheduled archive
///   working?" — reported `installed: true` from file presence alone. So for
///   systemd every expected timer file being present is only half the answer;
///   [`ScheduleInstall::Installed`] additionally requires the manager to
///   confirm each timer active, and files the manager did not confirm are
///   [`ScheduleInstall::Unconfirmed`] — the caller reports them with the
///   reason [`next_run`] carries (the manager answered "not armed", or could
///   not be asked at all), so "could not ask" never collapses into "not
///   armed".
/// * **launchd** (macOS) asks too. W287 measured the same defect here, on the
///   project's own primary platform: a `bootstrap` that failed left the plist
///   on disk, and `status` called it installed. So for launchd every expected
///   plist being present is only half the answer as well;
///   [`ScheduleInstall::Installed`] additionally requires `launchctl print` to
///   find each agent loaded, and a plist launchd did not confirm is
///   [`ScheduleInstall::Unconfirmed`]. The documented hand-install path is why
///   this matters on a machine nobody broke: `schedule --output` writes a plist
///   and loads nothing until the printed `bootstrap` command is run.
///
/// File presence stays half of the answer both formats because it is what the
/// installers create and the uninstallers remove; it is reported, never
/// silently substituted for the manager's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleInstall {
    /// Every expected unit file exists, and the scheduler confirmed the job is
    /// loaded where the format gets an answer (systemd: every timer active
    /// under `systemctl --user is-active`; launchd: every agent present under
    /// `launchctl print`).
    Installed,
    /// No expected unit file exists.
    NotInstalled,
    /// Some expected unit files exist and some do not: an interrupted install
    /// or a hand-removed unit, which is neither of the two clean states.
    Partial { present: usize, expected: usize },
    /// Every expected unit file exists and the manager did not confirm the job
    /// loaded — it answered that it was not, or it could not be asked. The
    /// file is not the schedule: a unit systemd has not loaded, or a plist
    /// launchd has not bootstrapped, protects nothing while looking exactly
    /// like one that does. `present` and `expected` stay in the answer so
    /// `Partial` and `Unconfirmed` can be told apart with the same fields;
    /// here they are equal by construction.
    Unconfirmed { present: usize, expected: usize },
}

/// The path of the timer/service file a `schedule install` for one
/// destination writes, for the format's own directory.
fn install_dir(format: Format, home: &Path) -> PathBuf {
    match format {
        Format::Launchd => home.join("Library/LaunchAgents"),
        Format::Systemd => home.join(".config/systemd/user"),
    }
}

/// One installed unit's on-disk path, named exactly as the installer names it.
fn unit_file_path(unit: Unit, format: Format, destination: Option<&str>, home: &Path) -> PathBuf {
    install_dir(format, home).join(unit_file_name(unit, format, destination))
}

/// The file name (not the path) of one unit, as the installer writes it.
pub fn unit_file_name(unit: Unit, format: Format, destination: Option<&str>) -> String {
    match format {
        Format::Launchd => {
            format!("{}.plist", launchd_label_for_destination(unit, destination))
        }
        Format::Systemd => systemd_timer_name_for_destination(unit, destination),
    }
}

/// Classify the state of the units in `targets`. An empty `targets` is
/// [`ScheduleInstall::NotInstalled`]: there is no unit to be present, and
/// calling that "installed" would invert the answer.
///
/// `tool` and `domain` are each run by one arm: `tool` is the format's own
/// manager (`systemctl` for systemd, `launchctl` for launchd) and `domain` is
/// the launchd domain the agents are registered in (see [`launchd_domain`]'s
/// caller). Both are parameters rather than lookups so a test can point the
/// probe at a throwaway fake instead of the machine's real session.
pub fn schedule_install_state(
    unit: Unit,
    format: Format,
    targets: &[Option<String>],
    home: &Path,
    tool: &Path,
    domain: &str,
) -> ScheduleInstall {
    if targets.is_empty() {
        return ScheduleInstall::NotInstalled;
    }
    let expected = targets.len();
    let present = targets
        .iter()
        .filter(|destination| unit_file_path(unit, format, destination.as_deref(), home).is_file())
        .count();
    if present == 0 {
        return ScheduleInstall::NotInstalled;
    }
    if present < expected {
        return ScheduleInstall::Partial { present, expected };
    }
    // Every unit file is present — half of the answer. The other half is the
    // manager's: the report's §2 had files on disk and no timer armed, and an
    // install state that stopped at the files is the false positive it filed.
    // Both formats get that answer now; W287 measured the same false positive
    // on launchd, where a failed `bootstrap` left the plist behind.
    let confirmed = match format {
        Format::Launchd => targets.iter().all(|destination| {
            launchd_agent_loaded(
                tool,
                domain,
                &launchd_label_for_destination(unit, destination.as_deref()),
            )
        }),
        Format::Systemd => targets.iter().all(|destination| {
            systemd_timer_active(
                tool,
                &systemd_timer_name_for_destination(unit, destination.as_deref()),
            )
        }),
    };
    if confirmed {
        ScheduleInstall::Installed
    } else {
        ScheduleInstall::Unconfirmed { present, expected }
    }
}

/// Whether `systemctl --user is-active` confirms one timer active — the same
/// question [`install_systemd_units`] asks itself before re-enabling, so the
/// state probe and the installer cannot disagree about what "armed" means.
///
/// A manager that cannot be asked (no `systemctl` on this machine, no user
/// session to reach) confirms nothing: that is not "inactive", it is no answer,
/// and the states stay distinguishable at the surface that prints the reason
/// ([`next_run`]'s note says "could not be run to ask", not "did not report
/// one").
fn systemd_timer_active(systemctl: &Path, timer: &str) -> bool {
    Command::new(systemctl)
        .args(["--user", "is-active", "--quiet", timer])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        // reason: a spawn failure is the absence of an answer, and the caller
        // reports "not confirmed" rather than "inactive" — the note beside the
        // state carries which one it was.
        .unwrap_or(false)
}

/// Whether launchd has one agent loaded — the same question
/// [`install_launchd_agents`] asks before it reloads one and
/// [`rollback_launchd_install`] asks before it unloads one, so the state probe
/// and the installer cannot disagree about what "loaded" means.
///
/// A launchctl that cannot be run (not installed, no GUI session to reach)
/// confirms nothing: that is not "not loaded", it is no answer, and the note
/// [`next_run`] carries says which of the two it was.
fn launchd_agent_loaded(launchctl: &Path, domain: &str, label: &str) -> bool {
    launchctl_status(launchctl, &format!("{domain}/{label}"))
        // reason: a spawn failure is the absence of an answer, and the caller
        // reports "not confirmed" rather than "not loaded" — the note beside
        // the state carries which one it was.
        .unwrap_or(false)
}

/// Why this agent cannot be said to be loaded, or `None` when it is.
///
/// The two failures [`NextRun`] must keep apart get a sentence each: launchd
/// answering "not loaded" is a fact about the schedule, and a `launchctl` that
/// could not be run at all is the absence of an answer. A caller that collapsed
/// them into a boolean would have to invent one of the two.
fn launchd_unloaded_note(launchctl: &Path, domain: &str, label: &str) -> Option<String> {
    let target = format!("{domain}/{label}");
    match launchctl_status(launchctl, &target) {
        Ok(true) => None,
        Ok(false) => Some(format!(
            "launchd has not loaded {label}, so it will not fire; `launchctl print {target}` \
             shows the agent's state, and `schedule install` loads it"
        )),
        Err(error) => Some(format!(
            "{} could not be run to ask whether {label} is loaded ({error}), so the next fire \
             time is unknown",
            launchctl.display()
        )),
    }
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
/// The launchd probe asks launchd first, for the same reason the systemd one
/// does: a plist's calendar slot is a fire time only for an agent that is
/// loaded. A plist written by `schedule --output` and not yet bootstrapped —
/// a documented way to install by hand — declares a slot launchd will never
/// reach, so reporting it as the next run would be the §2 false positive in
/// the field beside the install state rather than in it.
///
/// `tool` and `domain` name the format's own manager (`systemctl`, or
/// `launchctl` with the domain its agents live in), both as parameters so a
/// test can address a throwaway fake rather than the real session.
pub fn next_run(
    unit: Unit,
    format: Format,
    targets: &[Option<String>],
    home: &Path,
    tool: &Path,
    domain: &str,
    now: DateTime<Local>,
) -> NextRun {
    match targets {
        [] => NextRun::Unknown(
            "no scheduler unit was installed, so there is no next run to report".to_string(),
        ),
        [destination] => match format {
            // The plist is the source of the *slot*; launchd is the source of
            // whether the slot will ever be reached. Neither answer is the
            // other: an unloaded agent has a slot and no next run, and a
            // manager that cannot be asked leaves the next run unknown rather
            // than absent.
            Format::Launchd => {
                let label = launchd_label_for_destination(unit, destination.as_deref());
                match launchd_unloaded_note(tool, domain, &label) {
                    Some(note) => NextRun::Unknown(note),
                    None => launchd_next_run(unit, destination.as_deref(), home, now),
                }
            }
            Format::Systemd => systemd_next_run(unit, destination.as_deref(), tool),
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
             naming a Minute, so no next fire time can be derived from it",
            path.display()
        ));
    };
    if let Some(refusal) = plist_calendar_refusal(&text) {
        return NextRun::Unknown(format!(
            "the installed plist {} declares {refusal}, so no next fire time can be derived from \
             it",
            path.display()
        ));
    }
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
/// the fire time to one nobody asked for. Which fields a dict may carry at all
/// is [`plist_calendar_refusal`]'s question, asked before this one.
fn plist_calendar_slot(text: &str) -> Option<CalendarSlot> {
    let after_key = text.split_once("<key>StartCalendarInterval</key>")?.1;
    let dict = after_key.split_once("</dict>")?.0;
    let minute = u32::try_from(plist_integer(dict, "Minute")?).ok()?;
    if minute > 59 {
        return None;
    }
    let weekday = match plist_integer(dict, "Weekday") {
        Some(w) => {
            let val = u32::try_from(w).ok()?;
            if val > 7 {
                return None;
            }
            Some(val)
        }
        None => None,
    };
    let hour = match plist_integer(dict, "Hour") {
        Some(h) => {
            let val = u32::try_from(h).ok()?;
            if val > 23 {
                return None;
            }
            Some(val)
        }
        None => None,
    };
    Some(CalendarSlot {
        weekday,
        hour,
        minute,
    })
}

/// Why a `StartCalendarInterval` dict is a shape this tool cannot read, if it
/// is one; `None` is a slot [`next_calendar_occurrence`] can turn into a time.
///
/// The readable shapes are exactly the two the renderers write: a `Minute`
/// alone (one fire per hour) and `Weekday` with `Hour` and `Minute` (one fire a
/// week). Everything else is refused rather than partly read, because a partly
/// read dict states a fire time its other fields take back:
///
/// * a `Day`, `Month` or `Week` narrows the slot past the fields read here;
/// * an array of dicts declares one slot per dict, and this report carries one
///   next run;
/// * a `Weekday` without an `Hour`, or the reverse, is half of a slot.
///
/// Nothing else can tell what launchd will do with such a plist, so the answer
/// is that there is no fire time to state — never the time the readable half
/// implies.
fn plist_calendar_refusal(text: &str) -> Option<String> {
    let after_key = text.split_once("<key>StartCalendarInterval</key>")?.1;
    let dict = after_key.split_once("</dict>")?.0;
    if dict.trim_start().starts_with("<array>") {
        return Some("a StartCalendarInterval array, one slot for each dict it holds".to_string());
    }
    let mut weekday = false;
    let mut hour = false;
    let mut rest = dict;
    while let Some((_, after)) = rest.split_once("<key>") {
        let Some((field, tail)) = after.split_once("</key>") else {
            break;
        };
        match field {
            "Weekday" => weekday = true,
            "Hour" => hour = true,
            "Minute" => {}
            other => {
                return Some(format!(
                    "a StartCalendarInterval naming a {other} beside the Weekday, Hour and Minute \
                     this tool reads"
                ))
            }
        }
        rest = tail;
    }
    match (weekday, hour) {
        (true, false) => {
            Some("a StartCalendarInterval naming a Weekday but not an Hour".to_string())
        }
        (false, true) => {
            Some("a StartCalendarInterval naming an Hour but not a Weekday".to_string())
        }
        _ => None,
    }
}

struct CalendarSlot {
    weekday: Option<u32>,
    hour: Option<u32>,
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
    if let (Some(weekday), Some(hour)) = (slot.weekday, slot.hour) {
        // launchd accepts 0 and 7 for Sunday; chrono counts from Sunday at 0.
        let target = weekday % 7;
        let today = now.date_naive();
        let day = today.checked_add_days(Days::new(u64::from(
            (target + 7 - today.weekday().num_days_from_sunday()) % 7,
        )))?;
        match local_at(day, hour, slot.minute)? {
            this_week if this_week > now => Some(this_week),
            _ => local_at(day.checked_add_days(Days::new(7))?, hour, slot.minute),
        }
    } else if slot.weekday.is_none() && slot.hour.is_none() {
        next_hourly_occurrence(slot.minute, now)
    } else {
        None
    }
}

fn next_hourly_occurrence(minute: u32, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let today = now.date_naive();
    let current_hour = now.hour();
    for hours_ahead in 0..48u32 {
        let naive_base = today.and_hms_opt(current_hour, minute, 0)?;
        let candidate_naive =
            naive_base.checked_add_signed(chrono::Duration::hours(i64::from(hours_ahead)))?;
        match Local.from_local_datetime(&candidate_naive) {
            LocalResult::Single(dt) if dt > now => return Some(dt),
            LocalResult::Ambiguous(first, second) => {
                if first > now {
                    return Some(first);
                } else if second > now {
                    return Some(second);
                }
            }
            _ => continue,
        }
    }
    None
}

/// `date` at `hour`:`minute` in the local zone.
fn local_at(date: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Local>> {
    let naive = date.and_hms_opt(hour, minute, 0)?;
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

/// Deterministically pick a minute of the hour (0..=59) derived from a machine identifier.
///
/// Spreading machines across different minutes of the hour prevents several machines
/// from hitting the same destination (e.g. S3, R2, or SSH) at the exact same minute.
pub fn hourly_minute_for_machine(seed: &str) -> u32 {
    let digest = Sha256::digest(seed.as_bytes());
    let bytes: [u8; 4] = [digest[0], digest[1], digest[2], digest[3]];
    u32::from_be_bytes(bytes) % 60
}

/// Resolve the machine seed used for deterministic hourly minute selection.
///
/// The identity file is read from [`crate::config::default_data_root`] —
/// the one spelling of the data root — because that is where every writer
/// puts it. A second spelling here would read a path the identity is never
/// written to, and when `$XDG_DATA_HOME` is unset it is the same path
/// again. `home` is deliberately not a parameter: the render `home` is
/// `config::home_dir()`, which `default_data_root` already derives from.
///
/// `--machine`, then the identity file, then the platform's own machine id:
/// each is a value this machine has and its neighbours generally do not, which
/// is all the seed is for. A machine with none of the three shares the literal
/// seed below, and with it a minute — a seed that changed between renders would
/// rewrite the installed plist on every `schedule install`, so one constant
/// value is the price of a stable minute for a machine that cannot be told
/// apart; `--machine` is how such a machine gets a minute of its own.
fn resolve_machine_seed(args: &RunOnceArgs) -> String {
    if let Some(machine) = &args.machine {
        if !machine.is_empty() {
            return machine.clone();
        }
    }
    let id_path = crate::config::default_data_root().join("machine-identity");
    if let crate::identity::IdentityFileState::Loaded(id) =
        crate::identity::load_identity_state(&id_path)
    {
        return id.as_hex();
    }
    // Check hostname / platform machine id
    if let Some(m) = crate::id::machine_id() {
        if !m.is_empty() {
            return m;
        }
    }
    // Deterministic fallback
    "chat-stasher".to_string()
}

fn render_launchd(
    binary: &Path,
    stage: &Path,
    interval: u64,
    args: &RunOnceArgs,
    home: &Path,
    label: &str,
) -> String {
    render_launchd_job(
        &run_once_argv(binary, stage, args),
        interval,
        home,
        label,
        "run-once",
    )
}

fn render_launchd_job(
    argv: &[String],
    interval: u64,
    home: &Path,
    label: &str,
    log_name: &str,
) -> String {
    let success_note = if log_name == "run-once" {
        "exit 0 is success; result=NOOP means no snapshot, result=COMPLETED means snapshot created; non-zero is error."
    } else {
        "exit 0 is success; non-zero is refusal or incomplete work."
    };
    let log_dir = home.join("Library/Logs/chat-stasher");
    let stdout_log = log_dir.join(format!("{log_name}.log"));
    let stderr_log = log_dir.join(format!("{log_name}.err.log"));

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

    let schedule_key = if interval == HOURLY_INTERVAL_SECS {
        let seed = resolve_machine_seed(args);
        let minute = hourly_minute_for_machine(&seed);
        format!(
            "  <!-- Hourly at a deterministic minute per machine (:{minute:02}) so passes do not drift by their own duration and multiple machines do not hit a destination at the same minute. -->\n  <key>StartCalendarInterval</key>\n  <dict>\n    <key>Minute</key>\n    <integer>{minute}</integer>\n  </dict>"
        )
    } else {
        format!("  <key>StartInterval</key>\n  <integer>{interval}</integer>")
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- chat-stasher is a one-shot process: {success_note} -->
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}
  </array>
{schedule_key}
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
        schedule_key = schedule_key,
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
    render_systemd_job(
        &run_once_argv(binary, stage, args),
        service_name,
        "archive cycle",
    )
}

fn render_systemd_job(argv: &[String], service_name: &str, description: &str) -> String {
    let success_note = if description == "archive cycle" {
        "# exit 0 = success: result=NOOP means no snapshot; result=COMPLETED means snapshot created.\n# Non-zero = error; read the result line in the journal."
    } else {
        "# exit 0 = success; non-zero = refusal or incomplete work.\n# Read the command report in the journal."
    };
    let args = argv
        .iter()
        .map(|arg| systemd_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"[Unit]
Description=Run one chat-stasher {description} ({service_name})

[Service]
Type=oneshot
{success_note}
ExecStart={args}
SuccessExitStatus=0
StandardOutput=journal
StandardError=journal
"#
    )
}

fn render_systemd_timer(interval: u64, service_name: &str) -> String {
    render_systemd_interval_timer(interval, service_name, "Hourly chat-stasher archive cycle")
}

fn render_systemd_interval_timer(interval: u64, service_name: &str, description: &str) -> String {
    format!(
        r#"[Unit]
Description={description}

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

    /// The launchd domain the tests address their fake `launchctl` in. The
    /// systemd arms ignore it; the launchd ones need it to be what the fake
    /// answers for, and it is deliberately not a real target so a test that
    /// ever reached the machine's own launchd would find nothing rather than
    /// unload something.
    const TEST_DOMAIN: &str = "gui/test";

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

    /// Hourly launchd unit renders StartCalendarInterval with a deterministic minute,
    /// RunAtLoad=false, and jitter preamble, instead of StartInterval.
    #[test]
    fn launchd_hourly_plist_renders_start_calendar_interval() {
        let args = RunOnceArgs {
            machine: Some("machine-alpha".to_string()),
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
        let plist = &files[0].content;
        assert!(plist.contains("<key>StartCalendarInterval</key>"));
        assert!(!plist.contains("<key>StartInterval</key>"));
        assert!(!plist.contains("<key>Hour</key>"));
        assert!(!plist.contains("<key>Weekday</key>"));
        let minute = hourly_minute_for_machine("machine-alpha");
        assert!(plist.contains(&format!(
            "<key>Minute</key>\n    <integer>{minute}</integer>"
        )));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <false/>"));
        assert!(plist.contains("jitter="));
    }

    /// Non-hourly intervals (backup_interval_secs != 3600) fall back to StartInterval.
    #[test]
    fn launchd_non_hourly_plist_falls_back_to_start_interval() {
        let args = RunOnceArgs::default();
        let files = render(
            Unit::RunOnce,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            1800,
            &args,
            &ReclaimStageArgs::default(),
            Path::new("/home/tester"),
        );
        let plist = &files[0].content;
        assert!(!plist.contains("<key>StartCalendarInterval</key>"));
        assert!(plist.contains("<key>StartInterval</key>\n  <integer>1800</integer>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <false/>"));
        assert!(plist.contains("jitter="));
    }

    /// The hourly minute is derived deterministically from the machine ID and distributes across 0..=59.
    #[test]
    fn deterministic_hourly_minute_derived_from_machine_id() {
        let min_a = hourly_minute_for_machine("host-a");
        let min_b = hourly_minute_for_machine("host-b");
        assert!(min_a < 60);
        assert!(min_b < 60);
        assert_eq!(min_a, hourly_minute_for_machine("host-a"));
        assert_ne!(min_a, min_b);
    }

    /// The hourly StartCalendarInterval plist yields the exact next occurrence in local time.
    #[test]
    fn launchd_hourly_calendar_plist_yields_the_exact_next_local_occurrence() {
        use chrono::Timelike;

        let home = tempfile::tempdir().unwrap();
        let args = RunOnceArgs {
            machine: Some("machine-alpha".to_string()),
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
            home.path(),
        );
        write_plists(home.path(), &files);

        let now = local_noon();
        let next = launchd_next_run(Unit::RunOnce, None, home.path(), now);
        let value = next
            .value()
            .expect("the hourly plist names a calendar slot");
        assert_eq!(next.note(), None, "a known time carries no excuse");

        let when = DateTime::parse_from_str(value, "%a %Y-%m-%d %H:%M:%S %z")
            .expect("the reported value is a local timestamp");
        let expected_minute = hourly_minute_for_machine("machine-alpha");
        assert_eq!(when.minute(), expected_minute);
        assert!(when > now, "the next occurrence is never in the past");
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
    fn configured_cargo_target_is_disposable_regardless_of_its_directory_name() {
        let temp = tempfile::tempdir().expect("create test directory");
        let target = temp.path().join("shared-builds");
        for profile in ["debug", "release", "aarch64-apple-darwin/debug"] {
            let binary = target.join(profile).join("chat-stasher");
            assert!(is_build_artifact_in_target(&binary, Some(&target)));
            assert!(!is_build_artifact_in_target(&binary, None));
        }
        let installed = temp.path().join("bin/chat-stasher");
        assert!(!is_build_artifact_in_target(&installed, Some(&target)));
        let sibling = temp
            .path()
            .join("shared-builds-installed/debug/chat-stasher");
        assert!(!is_build_artifact_in_target(&sibling, Some(&target)));
        assert!(!is_build_artifact_in_target(
            &installed,
            Some(Path::new(""))
        ));
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
        crate::test_support::plant_executable(&script, &script_body);

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
        crate::test_support::plant_executable(&script, &script_body);

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

    /// The unit pair `render` writes for one destination without one, planted
    /// under a temp home so several roll-back tests below read the same shape
    /// the installer writes.
    fn run_once_units() -> Vec<TemplateFile> {
        vec![
            TemplateFile {
                name: "chat-stasher-run-once.service".into(),
                content: "service-v1".into(),
            },
            TemplateFile {
                name: "chat-stasher-run-once.timer".into(),
                content: "timer-v1".into(),
            },
        ]
    }

    fn units_dir(home: &Path) -> PathBuf {
        home.join(".config/systemd/user")
    }

    /// W282 §2, the report's own repro: with no user systemd session (PID 1 is
    /// init), `schedule install --format systemd` wrote both unit files and
    /// then failed at `daemon-reload`. The files it left behind made
    /// `schedule_install_state` — and so `status` — report an installed timer
    /// that was armed nowhere: a protected archive that was not protected. The
    /// install must instead leave the machine as it found it.
    #[cfg(unix)]
    #[test]
    fn a_daemon_reload_failure_leaves_no_unit_files_behind() {
        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("systemctl");
        let script_body =
            "#!/bin/sh\ncase \"$2\" in\n  daemon-reload) exit 1;;\n  *) exit 0;;\nesac\n";
        crate::test_support::plant_executable(&script, script_body);

        let install = install_systemd_units(temp.path(), &run_once_units(), &script);
        assert!(
            install.is_err(),
            "a manager that refuses to reload is an install failure: {install:?}"
        );
        let units = units_dir(temp.path());
        for name in [
            "chat-stasher-run-once.service",
            "chat-stasher-run-once.timer",
        ] {
            assert!(
                !units.join(name).is_file(),
                "the failed install must not leave {name} behind (W282 §2: the leftover file \
                 made status report an installed timer that was armed nowhere)"
            );
        }
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &[None],
                temp.path(),
                &script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::NotInstalled,
            "a failed install is no install; status must report the machine as the failed \
             install left it"
        );
    }

    /// The same defect one step later: `daemon-reload` succeeds but arming the
    /// timer fails (no user session to enable it into), still after both files
    /// were written. The roll-back obligation is the same, and so is the
    /// status claim it protects.
    #[cfg(unix)]
    #[test]
    fn an_enable_failure_leaves_no_unit_files_behind() {
        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("systemctl");
        let script_body = "#!/bin/sh\ncase \"$2\" in\n  enable) exit 1;;\n  *) exit 0;;\nesac\n";
        crate::test_support::plant_executable(&script, script_body);

        let install = install_systemd_units(temp.path(), &run_once_units(), &script);
        assert!(
            install.is_err(),
            "a manager that will not arm the timer is an install failure: {install:?}"
        );
        let units = units_dir(temp.path());
        for name in [
            "chat-stasher-run-once.service",
            "chat-stasher-run-once.timer",
        ] {
            assert!(
                !units.join(name).is_file(),
                "the failed install must not leave {name} behind after a failed enable either"
            );
        }
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &[None],
                temp.path(),
                &script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::NotInstalled
        );
    }

    /// A failed *re*install must restore the units it replaced, byte for byte:
    /// a broken upgrade must not delete the working timer's definition along
    /// with its own. This is the `daemon-reload` failure (the report's first
    /// repro) on top of a previous successful install, which is the shape a
    /// re-run after a renamed binary or a changed cadence takes.
    #[cfg(unix)]
    #[test]
    fn a_failed_reinstall_restores_the_units_it_replaced() {
        let temp = tempfile::tempdir().expect("create test directory");
        let units = units_dir(temp.path());
        fs::create_dir_all(&units).expect("create systemd unit directory");
        fs::write(units.join("chat-stasher-run-once.service"), "service-v0").unwrap();
        fs::write(units.join("chat-stasher-run-once.timer"), "timer-v0").unwrap();
        let script = temp.path().join("systemctl");
        let script_body =
            "#!/bin/sh\ncase \"$2\" in\n  daemon-reload) exit 1;;\n  *) exit 0;;\nesac\n";
        crate::test_support::plant_executable(&script, script_body);

        let upgraded = vec![
            TemplateFile {
                name: "chat-stasher-run-once.service".into(),
                content: "service-v1-new-binary".into(),
            },
            TemplateFile {
                name: "chat-stasher-run-once.timer".into(),
                content: "timer-v1-new-cadence".into(),
            },
        ];
        let install = install_systemd_units(temp.path(), &upgraded, &script);
        assert!(
            install.is_err(),
            "the manager refuses the reload: {install:?}"
        );
        assert_eq!(
            fs::read_to_string(units.join("chat-stasher-run-once.service")).expect(
                "the pre-existing service file must still be there, restored rather than \
                 deleted by the failed upgrade"
            ),
            "service-v0"
        );
        assert_eq!(
            fs::read_to_string(units.join("chat-stasher-run-once.timer"))
                .expect("the pre-existing timer file must still be there, restored"),
            "timer-v0"
        );
    }

    /// With more than one destination, `enable --now` runs one timer at a
    /// time. A failure on the second must also stop the first: a roll-back
    /// that leaves a timer it had already armed is a half install, and the
    /// armed half is the dangerous one — the machine looks installed to the
    /// process that is running, after the command that owns it failed.
    #[cfg(unix)]
    #[test]
    fn a_failed_multi_destination_install_stops_the_timers_it_had_armed() {
        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("systemctl");
        let log = temp.path().join("calls");
        let script_body = format!(
            "#!/bin/sh\n\
             echo \"$@\" >> \"{}\"\n\
             case \"$2\" in\n\
               enable) case \"$4\" in\n\
                 chat-stasher-run-once-beta.timer) exit 1;;\n\
                 *) exit 0;;\n\
               esac;;\n\
               *) exit 0;;\n\
             esac\n",
            log.display()
        );
        crate::test_support::plant_executable(&script, &script_body);

        let files = vec![
            TemplateFile {
                name: "chat-stasher-run-once-alpha.service".into(),
                content: "alpha-service".into(),
            },
            TemplateFile {
                name: "chat-stasher-run-once-alpha.timer".into(),
                content: "alpha-timer".into(),
            },
            TemplateFile {
                name: "chat-stasher-run-once-beta.service".into(),
                content: "beta-service".into(),
            },
            TemplateFile {
                name: "chat-stasher-run-once-beta.timer".into(),
                content: "beta-timer".into(),
            },
        ];
        let install = install_systemd_units(temp.path(), &files, &script);
        assert!(install.is_err(), "beta refuses to arm: {install:?}");
        let units = units_dir(temp.path());
        for name in [
            "chat-stasher-run-once-alpha.service",
            "chat-stasher-run-once-alpha.timer",
            "chat-stasher-run-once-beta.service",
            "chat-stasher-run-once-beta.timer",
        ] {
            assert!(
                !units.join(name).is_file(),
                "the failed install must not leave {name} behind"
            );
        }
        let calls = fs::read_to_string(log).expect("read fake systemctl log");
        assert_eq!(
            calls
                .matches("disable --now chat-stasher-run-once-alpha.timer")
                .count(),
            1,
            "the timer that was armed before the failure must be stopped by the roll-back: \
             {calls}"
        );
    }

    /// A fake `launchctl` for the read-only probes: `print` always answers
    /// `loaded`, and every other verb exits 2 — a probe that loaded or unloaded
    /// something would be a defect, and the log makes that visible.
    ///
    /// Its `print` ignores the target, which is the point: these tests are
    /// about the *answer*, not about which domain the probe built.
    #[cfg(unix)]
    fn launchd_answers(dir: &Path, loaded: bool) -> PathBuf {
        let script = dir.join("launchctl");
        let log = dir.join("probe-calls");
        let answer = if loaded { "exit 0" } else { "exit 1" };
        crate::test_support::plant_executable(
            &script,
            &format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\ncase \"$1\" in\n  print) {answer} ;;\n  *) \
                 exit 2 ;;\nesac\n",
                log.display()
            ),
        );
        script
    }

    /// A plist file left behind by a failed install is exactly what W287 §2
    /// reported on macOS: a failed `bootstrap` after the plist was written, a
    /// `status` that called the leftover plist an install, and an archive
    /// nobody was archiving. The install must leave the machine as it found it.
    #[cfg(unix)]
    #[test]
    fn a_failed_launchd_load_leaves_no_plist_behind() {
        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("launchctl");
        let loaded = temp.path().join("loaded");
        let script_body = format!(
            "#!/bin/sh\ncase \"$1\" in\n  print) test -f \"{}\" ;;\n  bootstrap) exit 1 ;;\n  \
             bootout) rm -f \"{}\" ;;\n  *) exit 2 ;;\nesac\n",
            loaded.display(),
            loaded.display()
        );
        crate::test_support::plant_executable(&script, &script_body);

        let file = TemplateFile {
            name: "com.chat-stasher.run-once.plist".to_string(),
            content: "plist-v1".to_string(),
        };
        let install = install_launchd_agents(temp.path(), &[file], &script, TEST_DOMAIN);
        assert!(
            install.is_err(),
            "a launchd that refuses the load is an install failure: {install:?}"
        );
        assert!(
            !temp
                .path()
                .join("Library/LaunchAgents/com.chat-stasher.run-once.plist")
                .is_file(),
            "the failed install must not leave the plist behind (W287: the leftover plist made \
             status report an agent that launchd had never loaded)"
        );
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &[None],
                temp.path(),
                &script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::NotInstalled,
            "a failed install is no install; status must report the machine as the failed \
             install left it"
        );
    }

    /// The re-install shape, which is the one that can cost a user a working
    /// schedule: the agent was loaded and serving, the new plist is written,
    /// and the load of the replacement fails. The roll-back must restore the
    /// previous bytes *and* load them again — a machine left with the old agent
    /// unloaded is not the machine this call found.
    #[cfg(unix)]
    #[test]
    fn a_failed_launchd_reinstall_restores_the_plist_and_loads_it_again() {
        let temp = tempfile::tempdir().expect("create test directory");
        let agents = temp.path().join("Library/LaunchAgents");
        fs::create_dir_all(&agents).expect("create launchd agent directory");
        fs::write(agents.join("com.chat-stasher.run-once.plist"), "plist-v0")
            .expect("write the pre-existing plist");
        let script = temp.path().join("launchctl");
        let loaded = temp.path().join("loaded");
        // The first load refuses — the reported repro — and every later one
        // succeeds, so the roll-back's own re-load is the load that is observed.
        let once = temp.path().join("refused-once");
        let log = temp.path().join("calls");
        let script_body = format!(
            "#!/bin/sh\necho \"$@\" >> \"{log}\"\ncase \"$1\" in\n  print) test -f \"{loaded}\" \
             ;;\n  bootstrap) if [ -f \"{once}\" ]; then touch \"{loaded}\"; else touch \
             \"{once}\"; exit 1; fi ;;\n  bootout) rm -f \"{loaded}\" ;;\n  *) exit 2 ;;\nesac\n",
            log = log.display(),
            loaded = loaded.display(),
            once = once.display()
        );
        crate::test_support::plant_executable(&script, &script_body);
        fs::write(&loaded, b"").expect("mark the pre-existing agent as loaded");

        let upgraded = TemplateFile {
            name: "com.chat-stasher.run-once.plist".to_string(),
            content: "plist-v1-new-binary".to_string(),
        };
        let install = install_launchd_agents(temp.path(), &[upgraded], &script, TEST_DOMAIN);
        assert!(install.is_err(), "the first load refuses: {install:?}");
        assert_eq!(
            fs::read_to_string(agents.join("com.chat-stasher.run-once.plist"))
                .expect("the pre-existing plist must still be there, restored"),
            "plist-v0",
            "the failed upgrade must put the working plist back, not delete it"
        );
        assert!(
            loaded.is_file(),
            "the agent that was loaded before this install must be loaded again by the roll-back"
        );
        let calls = fs::read_to_string(log).expect("read fake launchctl log");
        assert_eq!(
            calls.matches("bootstrap").count(),
            2,
            "one refused load, then the roll-back's own: {calls}"
        );
        assert_eq!(
            calls.matches(&format!("bootstrap {TEST_DOMAIN}")).count(),
            2,
            "both loads name the domain the install was given: {calls}"
        );
    }

    /// The multi-destination shape: the first agent loads, the second does not.
    /// A roll-back that leaves the first one loaded leaves a half install, and
    /// the loaded half is the dangerous one — launchd keeps running a schedule
    /// the command that created it reported as failed.
    ///
    /// The fake keeps one marker per label rather than one for the whole
    /// manager, because "is this agent loaded?" is the question under test: a
    /// single marker would say alpha is loaded when alpha is the only agent
    /// that failed to load.
    #[cfg(unix)]
    #[test]
    fn a_failed_multi_destination_launchd_install_unloads_what_it_loaded() {
        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("launchctl");
        let state = temp.path().join("state");
        fs::create_dir_all(&state).expect("create the fake manager's state directory");
        let log = temp.path().join("calls");
        let script_body = format!(
            "#!/bin/sh\necho \"$@\" >> \"{log}\"\ncase \"$1\" in\n  \
             print) test -f \"{state}/$(basename \"$2\")\" ;;\n  \
             bootstrap) label=$(basename \"$3\" .plist); case \"$label\" in *beta*) exit 1;; \
             esac; touch \"{state}/$label\" ;;\n  \
             bootout) rm -f \"{state}/$(basename \"$3\")\" ;;\n  *) exit 2 ;;\nesac\n",
            log = log.display(),
            state = state.display()
        );
        crate::test_support::plant_executable(&script, &script_body);

        let files = vec![
            TemplateFile {
                name: "com.chat-stasher.run-once.alpha.plist".to_string(),
                content: "alpha-v1".to_string(),
            },
            TemplateFile {
                name: "com.chat-stasher.run-once.beta.plist".to_string(),
                content: "beta-v1".to_string(),
            },
        ];
        let install = install_launchd_agents(temp.path(), &files, &script, TEST_DOMAIN);
        assert!(install.is_err(), "beta refuses to load: {install:?}");
        let agents = temp.path().join("Library/LaunchAgents");
        for name in [
            "com.chat-stasher.run-once.alpha.plist",
            "com.chat-stasher.run-once.beta.plist",
        ] {
            assert!(
                !agents.join(name).is_file(),
                "the failed install must not leave {name} behind"
            );
        }
        assert!(
            fs::read_dir(&state)
                .expect("read the fake manager's state")
                .next()
                .is_none(),
            "no agent may still be loaded after the roll-back"
        );
        let calls = fs::read_to_string(log).expect("read fake launchctl log");
        assert_eq!(
            calls
                .matches(&format!(
                    "bootout {TEST_DOMAIN} com.chat-stasher.run-once.alpha"
                ))
                .count(),
            1,
            "the agent this failed install had already loaded must be unloaded by the \
             roll-back: {calls}"
        );
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

    /// The timestamp half of a systemd `Trigger:` line — weekday, date, time and
    /// zone abbreviation — three days after the clock the caller injects.
    ///
    /// Derived rather than written down. [`systemd_trigger_line`] reads the
    /// `Trigger:` line and its `; ` delimiter and never the calendar, so a
    /// literal date in a fixture here is decoration — and decoration that ages
    /// into a date in the past reads, wrongly, as a deadline the test depends on.
    /// `now` is the test's own fixture clock ([`local_noon`]) and never the wall
    /// clock, so the fixture stays deterministic while its date stays current.
    fn trigger_stamp(now: DateTime<Local>, zone: &str) -> String {
        format!(
            "{} 03:17:00 {zone}",
            (now + Days::new(3)).format("%a %Y-%m-%d")
        )
    }

    /// A calendar plist is its own source: the fire time is computed from the
    /// `StartCalendarInterval` keys the plist carries, on the local calendar.
    ///
    /// The plist tests below call [`launchd_next_run`] rather than going
    /// through [`next_run`]: since W287 the dispatcher asks launchd whether the
    /// agent is loaded before it reads the plist, and asking means executing a
    /// manager, which a test can only fake with an executable script — unix
    /// only, and launchd is macOS-only in any case (`setup_schedule_format`
    /// never selects it elsewhere). Reading the plist is platform-neutral
    /// arithmetic and stays covered on every platform here; the dispatcher's
    /// half has its own tests, and the wizard suite drives it end to end
    /// against a real binary.
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
        let next = launchd_next_run(Unit::ReclaimStage, None, home.path(), now);
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
        let next = launchd_next_run(Unit::ReclaimStage, None, home.path(), now);
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
            1800,
            &RunOnceArgs::default(),
            &ReclaimStageArgs::default(),
            home.path(),
        );
        write_plists(home.path(), &files);

        let next = launchd_next_run(Unit::RunOnce, None, home.path(), local_noon());
        assert_eq!(next.value(), None);
        assert_eq!(
            next.note(),
            Some(
                "launchd interval jobs expose no next fire time; the job runs every 30 minutes \
                 after load"
            )
        );
    }

    #[test]
    fn a_partial_calendar_shape_is_reported_as_unreadable_not_as_a_dst_gap() {
        let home = tempfile::tempdir().unwrap();
        let agents = home.path().join("Library/LaunchAgents");
        fs::create_dir_all(&agents).expect("create launchd agent directory");
        let label = launchd_label_for_destination(Unit::RunOnce, None);
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>StartCalendarInterval</key>\n\
             \t<dict>\n\
             \t\t<key>Hour</key>\n\
             \t\t<integer>3</integer>\n\
             \t\t<key>Minute</key>\n\
             \t\t<integer>17</integer>\n\
             \t</dict>\n\
             </dict>\n\
             </plist>\n"
        );
        fs::write(agents.join(format!("{label}.plist")), plist).expect("write plist");

        let next = launchd_next_run(Unit::RunOnce, None, home.path(), local_noon());
        assert_eq!(next.value(), None);
        let note = next.note().expect("an empty answer carries its reason");
        assert!(
            !note.contains("daylight-saving"),
            "a partial shape is not a DST gap: note={note}"
        );
        assert!(
            note.contains("naming an Hour but not a Weekday"),
            "the missing half is named: note={note}"
        );
    }

    /// A calendar field this tool does not read must not be dropped on the way
    /// to a fire time.
    ///
    /// `Minute` alone is one fire per hour; `Minute` beside a `Day` is one fire
    /// on that date each month. Answering the first while the plist declares the
    /// second reports a time launchd was never asked for, so the dict goes
    /// unread — and the note names the field that caused it.
    #[test]
    fn a_calendar_field_this_tool_does_not_read_is_reported_not_ignored() {
        let home = tempfile::tempdir().unwrap();
        let agents = home.path().join("Library/LaunchAgents");
        fs::create_dir_all(&agents).expect("create launchd agent directory");
        let label = launchd_label_for_destination(Unit::RunOnce, None);
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>StartCalendarInterval</key>\n\
             \t<dict>\n\
             \t\t<key>Minute</key>\n\
             \t\t<integer>17</integer>\n\
             \t\t<key>Day</key>\n\
             \t\t<integer>4</integer>\n\
             \t</dict>\n\
             </dict>\n\
             </plist>\n"
        );
        fs::write(agents.join(format!("{label}.plist")), plist).expect("write plist");

        let next = launchd_next_run(Unit::RunOnce, None, home.path(), local_noon());
        assert_eq!(
            next.value(),
            None,
            "a slot narrowed by a field this tool does not read has no fire time it can state"
        );
        let note = next.note().expect("an empty answer carries its reason");
        assert!(
            note.contains("naming a Day"),
            "the field that cannot be read is named: note={note}"
        );
    }

    /// An array of dicts is several slots, and this report carries one next run:
    /// the first dict's slot is not the schedule, so there is no single time to
    /// state.
    #[test]
    fn a_calendar_interval_array_is_refused_rather_than_half_read() {
        let home = tempfile::tempdir().unwrap();
        let agents = home.path().join("Library/LaunchAgents");
        fs::create_dir_all(&agents).expect("create launchd agent directory");
        let label = launchd_label_for_destination(Unit::RunOnce, None);
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>StartCalendarInterval</key>\n\
             \t<array>\n\
             \t\t<dict>\n\
             \t\t\t<key>Minute</key>\n\
             \t\t\t<integer>17</integer>\n\
             \t\t</dict>\n\
             \t\t<dict>\n\
             \t\t\t<key>Minute</key>\n\
             \t\t\t<integer>47</integer>\n\
             \t\t</dict>\n\
             \t</array>\n\
             </dict>\n\
             </plist>\n"
        );
        fs::write(agents.join(format!("{label}.plist")), plist).expect("write plist");

        let next = launchd_next_run(Unit::RunOnce, None, home.path(), local_noon());
        assert_eq!(
            next.value(),
            None,
            "the schedule is every dict in the array, not the first one"
        );
        let note = next.note().expect("an empty answer carries its reason");
        assert!(
            note.contains("StartCalendarInterval array"),
            "the shape that cannot be read is named: note={note}"
        );
    }

    /// A plist that cannot be read is not a plist that declares nothing: the
    /// two must not collapse into one answer.
    #[test]
    fn an_unreadable_launchd_plist_is_reported_as_unread_not_as_absent() {
        let home = tempfile::tempdir().unwrap();
        let next = launchd_next_run(Unit::RunOnce, None, home.path(), local_noon());
        assert_eq!(next.value(), None);
        let note = next.note().expect("an empty answer carries its reason");
        assert!(note.contains("could not be read"), "note={note}");
        assert!(
            note.contains("com.chat-stasher.run-once.plist"),
            "the unreadable file is named: note={note}"
        );
    }

    /// Only the `Trigger:` line is a fire time, and only that line's timestamp:
    /// the `; 3 days left` tail the fixture carries must not reach the value.
    /// `n/a` is systemd's own spelling for "no next elapse" and must not be read
    /// as one.
    #[test]
    fn the_systemd_answer_comes_from_the_trigger_line_alone() {
        // Both stamps come from this module's fixture clock rather than being
        // written down, so the fixture is what systemd prints for a timer armed
        // three days out — and never a date that has already passed.
        let now = local_noon();
        let since = now.format("%a %Y-%m-%d %H:%M:%S CEST").to_string();
        let trigger = trigger_stamp(now, "CEST");
        let status = format!(
            "● chat-stasher-run-once.timer - Hourly chat-stasher archive cycle\n     \
                      Loaded: loaded (/home/u/.config/systemd/user/chat-stasher-run-once.timer)\n     \
                      Active: active (waiting) since {since}; 2h ago\n    \
                      Trigger: {trigger}; 3 days left\n   \
                      Triggers: ● chat-stasher-run-once.service\n");
        assert_eq!(
            systemd_trigger_line(&status).as_deref(),
            Some(trigger.as_str())
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
            TEST_DOMAIN,
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
            TEST_DOMAIN,
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
        let temp = tempfile::tempdir().expect("create test directory");
        let script = temp.path().join("systemctl");
        let now = local_noon();
        // The line systemd would print for a timer armed three days out: the
        // timestamp half comes from the clock this test injects, never from a
        // literal date, so the fixture cannot turn into a deadline that has
        // already passed. `printf` is a shell builtin, so the fake resolves
        // nothing through `PATH` and reads no second file either; the branch
        // under test is the parse.
        //
        // This note used to blame that `PATH` lookup for the `could not be run`
        // CI saw here. It was wrong: the error was ETXTBSY (`Text file busy`,
        // os error 26) — the kernel refusing to exec a file that still has an
        // open write descriptor, and the descriptor was this process's own,
        // inherited by a child another test thread forked while `fs::write` was
        // still open. See `crate::test_support`; the fixture is planted through
        // a child so no fork of ours can carry one.
        let trigger = trigger_stamp(now, "CEST");
        crate::test_support::plant_executable(
            &script,
            &format!("#!/bin/sh\nprintf '%s\\n' '    Trigger: {trigger}; 3 days left'\n"),
        );

        let known = next_run(
            Unit::RunOnce,
            Format::Systemd,
            &[None],
            temp.path(),
            &script,
            TEST_DOMAIN,
            now,
        );
        // The reason travels with the message: a bare `None` here cannot say
        // which of the two unknowns came back, and the fixture's own shell
        // answering nothing is one of them.
        assert_eq!(
            known.value(),
            Some(trigger.as_str()),
            "the scheduler's own text, minus its `; <relative time>` tail; note={:?}",
            known.note()
        );
        assert_eq!(known.note(), None);

        crate::test_support::plant_executable(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '    Trigger: n/a'\n",
        );
        let unarmed = next_run(
            Unit::RunOnce,
            Format::Systemd,
            &[None],
            temp.path(),
            &script,
            TEST_DOMAIN,
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
            TEST_DOMAIN,
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

    /// The states an install is in before the manager is asked at all — no
    /// files, some files, no expected files. None of them reaches a probe, so
    /// this stays a pure file read on every platform; the states that do ask
    /// launchd are the tests below.
    #[test]
    fn schedule_install_state_counts_the_unit_files() {
        let home = tempfile::TempDir::new().unwrap();
        let targets = vec![Some("a".to_string()), Some("b".to_string())];
        let dir = home.path().join("Library/LaunchAgents");
        fs::create_dir_all(&dir).unwrap();

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &targets,
                home.path(),
                Path::new("launchctl"),
                TEST_DOMAIN,
            ),
            ScheduleInstall::NotInstalled
        );
        let a = dir.join(format!(
            "{}.plist",
            launchd_label_for_destination(Unit::RunOnce, Some("a"))
        ));
        fs::write(&a, b"<plist/>").unwrap();
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &targets,
                home.path(),
                Path::new("launchctl"),
                TEST_DOMAIN,
            ),
            ScheduleInstall::Partial {
                present: 1,
                expected: 2
            }
        );

        // No expected unit is no install, never "trivially installed".
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &[],
                home.path(),
                Path::new("launchctl"),
                TEST_DOMAIN,
            ),
            ScheduleInstall::NotInstalled
        );
    }

    /// systemd reads the same state from the timer unit path — plus the
    /// manager's confirmation, without which two timer files alone are
    /// [`ScheduleInstall::Unconfirmed`] (see the tests below).
    #[cfg(unix)]
    #[test]
    fn schedule_install_state_reads_systemd_timer_files() {
        let home = tempfile::TempDir::new().unwrap();
        let targets = vec![None];
        let dir = units_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        let fake = ArmedFake::plant(home.path(), true);
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &targets,
                home.path(),
                &fake.script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::NotInstalled
        );
        fs::write(dir.join(SYSTEMD_TIMER), b"[Timer]\n").unwrap();
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &targets,
                home.path(),
                &fake.script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::Installed
        );
        assert_eq!(
            unit_file_name(Unit::RunOnce, Format::Systemd, None),
            SYSTEMD_TIMER
        );
    }

    /// A fake `systemctl` whose `is-active` answer the next tests control.
    /// One file stands for the manager's armed state, matching what a real
    /// manager answers for a timer it has *not* armed: a non-zero exit.
    #[cfg(unix)]
    struct ArmedFake {
        script: PathBuf,
    }

    #[cfg(unix)]
    impl ArmedFake {
        fn plant(dir: &Path, armed: bool) -> ArmedFake {
            let script = dir.join("systemctl");
            let body = if armed {
                "#!/bin/sh\ncase \"$2\" in is-active) exit 0;; *) exit 0;; esac\n"
            } else {
                "#!/bin/sh\ncase \"$2\" in is-active) exit 1;; *) exit 0;; esac\n"
            };
            crate::test_support::plant_executable(&script, body);
            ArmedFake { script }
        }
    }

    /// The report's §2 status half: all the unit files present, no timer
    /// armed, and `status` claiming `installed: true` from file presence
    /// alone. File presence is now only half the systemd answer — the manager
    /// must confirm each timer active for [`ScheduleInstall::Installed`], and
    /// anything else the files say is reported as [`ScheduleInstall::Unconfirmed`]
    /// rather than claimed as an install. The manager's own "not armed" answer
    /// is the case a user must be able to see instead of a lie.
    #[cfg(unix)]
    #[test]
    fn present_units_the_manager_did_not_confirm_are_unconfirmed() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = units_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SYSTEMD_SERVICE), b"[Service]\n").unwrap();
        fs::write(dir.join(SYSTEMD_TIMER), b"[Timer]\n").unwrap();
        let fake = ArmedFake::plant(home.path(), false);

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &[None],
                home.path(),
                &fake.script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::Unconfirmed {
                present: 1,
                expected: 1
            }
        );
    }

    /// The same files, a manager that confirms the timer — that is the one
    /// pair of facts an "installed" claim needs, so an install state that
    /// reported less than both would be the opposite defect: a healthy timer
    /// talked down. Both halves must be observable separately.
    #[cfg(unix)]
    #[test]
    fn present_units_the_manager_confirms_are_installed() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = units_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SYSTEMD_SERVICE), b"[Service]\n").unwrap();
        fs::write(dir.join(SYSTEMD_TIMER), b"[Timer]\n").unwrap();
        let fake = ArmedFake::plant(home.path(), true);

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &[None],
                home.path(),
                &fake.script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::Installed
        );
    }

    /// A manager that cannot be asked at all (WSL without a user systemd
    /// session, a `systemctl` that is not installed) is a third state, not
    /// "the manager said no": the answer's note carries which one it was, so
    /// the install state itself must not collapse the two into a claim
    /// stronger than either. Both report as unconfirmed here; the note —
    /// `next_run`'s — is what distinguishes them at the surface that prints
    /// it.
    #[cfg(unix)]
    #[test]
    fn an_unaskable_manager_still_gets_no_installed_claim() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = units_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SYSTEMD_SERVICE), b"[Service]\n").unwrap();
        fs::write(dir.join(SYSTEMD_TIMER), b"[Timer]\n").unwrap();

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &[None],
                home.path(),
                &home.path().join("no-such-systemctl"),
                TEST_DOMAIN,
            ),
            ScheduleInstall::Unconfirmed {
                present: 1,
                expected: 1
            }
        );
    }

    /// Multi-destination: every timer file present, and the manager confirming
    /// none of them. The fields stay in the answer so `Partial` and
    /// `Unconfirmed` remain distinguishable by the same two numbers, and the
    /// two unconfirmed timers count as files — as they always did.
    #[cfg(unix)]
    #[test]
    fn one_unconfirmed_timer_unconfirms_the_whole_install() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = units_dir(home.path());
        fs::create_dir_all(&dir).unwrap();
        for destination in ["alpha", "beta"] {
            let stem = format!("chat-stasher-run-once-{destination}");
            fs::write(dir.join(format!("{stem}.service")), b"[Service]\n").unwrap();
            fs::write(dir.join(format!("{stem}.timer")), b"[Timer]\n").unwrap();
        }
        let fake = ArmedFake::plant(home.path(), false);
        let targets = vec![Some("alpha".to_string()), Some("beta".to_string())];
        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Systemd,
                &targets,
                home.path(),
                &fake.script,
                TEST_DOMAIN,
            ),
            ScheduleInstall::Unconfirmed {
                present: 2,
                expected: 2
            }
        );
    }

    /// Write the plist `render` produces for one unit, where the installer
    /// writes it, so a probe reads the bytes launchd would.
    fn write_one_plist(home: &Path, unit: Unit) -> PathBuf {
        let files = render(
            unit,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            0,
            &RunOnceArgs::default(),
            &ReclaimStageArgs::default(),
            home,
        );
        write_plists(home, &files);
        home.join("Library/LaunchAgents")
            .join(unit_file_name(unit, Format::Launchd, None))
    }

    /// W287, §2 on macOS: the plist is on disk and launchd has not loaded it —
    /// the exact state a failed `bootstrap` leaves, and the state
    /// `schedule --output` leaves until the printed command is run. Calling
    /// that `installed` is the false positive the report filed.
    #[cfg(unix)]
    #[test]
    fn a_plist_launchd_did_not_load_is_unconfirmed() {
        let home = tempfile::TempDir::new().unwrap();
        write_one_plist(home.path(), Unit::RunOnce);
        let fake = launchd_answers(home.path(), false);

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &[None],
                home.path(),
                &fake,
                TEST_DOMAIN,
            ),
            ScheduleInstall::Unconfirmed {
                present: 1,
                expected: 1
            }
        );
    }

    /// The same plist with launchd confirming the agent loaded — the pair of
    /// facts an "installed" claim needs. An install state that reported less
    /// than both would be the opposite defect: a working agent talked down.
    #[cfg(unix)]
    #[test]
    fn a_plist_launchd_confirms_is_installed() {
        let home = tempfile::TempDir::new().unwrap();
        write_one_plist(home.path(), Unit::RunOnce);
        let fake = launchd_answers(home.path(), true);

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &[None],
                home.path(),
                &fake,
                TEST_DOMAIN,
            ),
            ScheduleInstall::Installed
        );
    }

    /// A `launchctl` that cannot be run at all confirms nothing either, and is
    /// a third state rather than "launchd said no": the note [`next_run`]
    /// carries says which of the two it was.
    #[cfg(unix)]
    #[test]
    fn an_unaskable_launchctl_still_gets_no_installed_claim() {
        let home = tempfile::TempDir::new().unwrap();
        write_one_plist(home.path(), Unit::RunOnce);

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &[None],
                home.path(),
                &home.path().join("no-such-launchctl"),
                TEST_DOMAIN,
            ),
            ScheduleInstall::Unconfirmed {
                present: 1,
                expected: 1
            }
        );
    }

    /// Multi-destination on launchd: both plists present, neither agent loaded.
    /// One unloaded agent is enough to unconfirm the install, for the same
    /// reason one unarmed timer is on systemd.
    #[cfg(unix)]
    #[test]
    fn one_unloaded_launchd_agent_unconfirms_the_whole_install() {
        let home = tempfile::TempDir::new().unwrap();
        let agents = home.path().join("Library/LaunchAgents");
        fs::create_dir_all(&agents).unwrap();
        for destination in ["alpha", "beta"] {
            fs::write(
                agents.join(format!(
                    "{}.plist",
                    launchd_label_for_destination(Unit::RunOnce, Some(destination))
                )),
                b"<plist/>",
            )
            .unwrap();
        }
        let fake = launchd_answers(home.path(), false);
        let targets = vec![Some("alpha".to_string()), Some("beta".to_string())];

        assert_eq!(
            schedule_install_state(
                Unit::RunOnce,
                Format::Launchd,
                &targets,
                home.path(),
                &fake,
                TEST_DOMAIN,
            ),
            ScheduleInstall::Unconfirmed {
                present: 2,
                expected: 2
            }
        );
    }

    /// A plist whose slot is a real calendar time does not make that time the
    /// next run unless launchd is going to reach it. The two assertions below
    /// are the same plist, the same clock and the same code — only the
    /// manager's answer differs, so the difference between a time and an
    /// explanation has to be the manager's answer.
    #[cfg(unix)]
    #[test]
    fn a_plist_launchd_did_not_load_reports_no_next_run() {
        let home = tempfile::TempDir::new().unwrap();
        write_one_plist(home.path(), Unit::ReclaimStage);
        let unloaded = launchd_answers(home.path(), false);

        let none = next_run(
            Unit::ReclaimStage,
            Format::Launchd,
            &[None],
            home.path(),
            &unloaded,
            TEST_DOMAIN,
            local_noon(),
        );
        assert_eq!(
            none.value(),
            None,
            "a calendar slot launchd will never reach is not a next run"
        );
        let note = none.note().expect("the empty answer carries its reason");
        assert!(
            note.contains("has not loaded"),
            "the manager answered that it is not loaded, and the note says so: {note}"
        );

        let loaded = launchd_answers(home.path(), true);
        let known = next_run(
            Unit::ReclaimStage,
            Format::Launchd,
            &[None],
            home.path(),
            &loaded,
            TEST_DOMAIN,
            local_noon(),
        );
        assert!(
            known.value().is_some(),
            "the same plist with the agent loaded is a time: {:?}",
            known.note()
        );
    }

    /// The other unknown: a `launchctl` that cannot be run is not a manager
    /// that answered "not loaded", and the note must not collapse the two.
    #[cfg(unix)]
    #[test]
    fn an_unaskable_launchctl_leaves_the_launchd_next_run_unknown() {
        let home = tempfile::TempDir::new().unwrap();
        write_one_plist(home.path(), Unit::ReclaimStage);

        let next = next_run(
            Unit::ReclaimStage,
            Format::Launchd,
            &[None],
            home.path(),
            &home.path().join("no-such-launchctl"),
            TEST_DOMAIN,
            local_noon(),
        );
        assert_eq!(next.value(), None);
        let note = next.note().expect("the empty answer carries its reason");
        assert!(
            note.contains("could not be run to ask"),
            "no answer is not the answer \"not loaded\": {note}"
        );
    }
}
