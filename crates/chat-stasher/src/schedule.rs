//! Render scheduler templates without installing or registering them.
//!
//! The scheduler is deliberately external to chat-stasher: launchd/systemd
//! starts one run-once process, which exits after the pass. Rendering remains
//! side-effect free; the launchd install helpers are explicit and testable.

use anyhow::{bail, Context, Result};
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

/// Single-quote a string for use inside a POSIX sh command. A single quote in
/// the input is closed, re-opened around a double-quoted quote, and continued.
fn sh_single_quote(value: &str) -> String {
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
}
