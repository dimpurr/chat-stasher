//! Render scheduler templates without installing or registering them.
//!
//! The scheduler is deliberately external to chat-stasher: launchd/systemd
//! starts one run-once process, which exits after the pass. This module only
//! renders files and the commands a human may choose to run later.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{Config, DEFAULT_BACKUP_INTERVAL_SECS};

pub const LAUNCHD_LABEL: &str = "com.chat-stasher.run-once";
pub const SYSTEMD_SERVICE: &str = "chat-stasher-run-once.service";
pub const SYSTEMD_TIMER: &str = "chat-stasher-run-once.timer";

/// Weekly `reap-stage` unit names. Deliberately separate from the run-once
/// names above: a launchd label / systemd unit name is the identity a restart
/// or teardown command refers to, so two timers must never share one.
pub const LAUNCHD_LABEL_REAP_STAGE: &str = "com.chat-stasher.reap-stage";
pub const SYSTEMD_SERVICE_REAP_STAGE: &str = "chat-stasher-reap-stage.service";
pub const SYSTEMD_TIMER_REAP_STAGE: &str = "chat-stasher-reap-stage.timer";

/// The weekly `reap-stage` slot: Sunday 03:17 local time.
///
/// `reap-stage` proves every archived shard against its destination before
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
/// ([`REAP_STAGE_RANDOMIZED_DELAY_SECS`]). The hourly run-once uses
/// `StartInterval`, whose phase is relative to load time, so no single minute
/// is guaranteed collision-free — but a weekly ~20 minute network-bound pass
/// occasionally overlapping the hourly pass is a non-event compared with
/// running it on top of interactive work.
pub const REAP_STAGE_WEEKDAY: u8 = 0; // launchd: 0 and 7 both mean Sunday.
pub const REAP_STAGE_HOUR: u8 = 3;
pub const REAP_STAGE_MINUTE: u8 = 17;

/// systemd `RandomizedDelaySec` for the weekly timer, in seconds: up to 15
/// minutes of random start delay so a fleet of machines does not all hit their
/// destination at 03:17:00 on the same second.
pub const REAP_STAGE_RANDOMIZED_DELAY_SECS: u64 = 15 * 60;

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
    /// Weekly `reap-stage --apply` stage reclamation. "Reap" here is stage
    /// reclamation (delete staged shard bodies once the archive proves it
    /// holds them) — unrelated to the ssh connection reaping that `--no-reap`
    /// disables.
    ReapStage,
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
    pub no_reap: bool,
    pub verify: bool,
}

/// Arguments forwarded from `schedule --unit reap-stage` to the embedded
/// `reap-stage` command. The stage path is separate — it is required in both
/// units — and `--apply` is always embedded (a weekly timer that only dry-ran
/// would report forever and delete nothing). The overrides here mirror
/// `reap-stage`'s own single-destination-only slots.
#[derive(Debug, Clone, Default)]
pub struct ReapStageArgs {
    pub repo: Option<String>,
    pub key_file: Option<String>,
    pub connections: Option<usize>,
    pub options: Vec<String>,
    pub no_reap: bool,
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
/// run-once renderers take `args` (and `interval`), the reap-stage renderers
/// take `reap_args` (the weekly slot is fixed, so `interval` is unused there).
pub fn render(
    unit: Unit,
    format: Format,
    binary: &Path,
    stage: &Path,
    interval: u64,
    args: &RunOnceArgs,
    reap_args: &ReapStageArgs,
    home: &Path,
) -> Vec<TemplateFile> {
    match unit {
        Unit::RunOnce => render_run_once(format, binary, stage, interval, args, home),
        Unit::ReapStage => render_reap_stage(format, binary, stage, reap_args, home),
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
            name: format!("{LAUNCHD_LABEL}.plist"),
            content: render_launchd(binary, stage, interval, args, home),
        }],
        Format::Systemd => vec![
            TemplateFile {
                name: SYSTEMD_SERVICE.to_string(),
                content: render_systemd_service(binary, stage, args),
            },
            TemplateFile {
                name: SYSTEMD_TIMER.to_string(),
                content: render_systemd_timer(interval),
            },
        ],
    }
}

fn render_reap_stage(
    format: Format,
    binary: &Path,
    stage: &Path,
    args: &ReapStageArgs,
    home: &Path,
) -> Vec<TemplateFile> {
    match format {
        Format::Launchd => vec![TemplateFile {
            name: format!("{LAUNCHD_LABEL_REAP_STAGE}.plist"),
            content: render_launchd_reap_stage(binary, stage, args, home),
        }],
        Format::Systemd => vec![
            TemplateFile {
                name: SYSTEMD_SERVICE_REAP_STAGE.to_string(),
                content: render_systemd_service_reap_stage(binary, stage, args),
            },
            TemplateFile {
                name: SYSTEMD_TIMER_REAP_STAGE.to_string(),
                content: render_systemd_timer_reap_stage(),
            },
        ],
    }
}

/// The launchd label for a unit. Pub so `cmd_schedule` can name the file a
/// saved plist should be copied to.
pub fn launchd_label(unit: Unit) -> &'static str {
    match unit {
        Unit::RunOnce => LAUNCHD_LABEL,
        Unit::ReapStage => LAUNCHD_LABEL_REAP_STAGE,
    }
}

pub fn systemd_service_name(unit: Unit) -> &'static str {
    match unit {
        Unit::RunOnce => SYSTEMD_SERVICE,
        Unit::ReapStage => SYSTEMD_SERVICE_REAP_STAGE,
    }
}

pub fn systemd_timer_name(unit: Unit) -> &'static str {
    match unit {
        Unit::RunOnce => SYSTEMD_TIMER,
        Unit::ReapStage => SYSTEMD_TIMER_REAP_STAGE,
    }
}

/// Write rendered templates. For launchd, output is the plist file. For
/// systemd, output is a directory containing the service and timer files.
pub fn write_templates(
    format: Format,
    output: &Path,
    files: &[TemplateFile],
) -> Result<Vec<PathBuf>> {
    if matches!(format, Format::Systemd) {
        fs::create_dir_all(output)
            .with_context(|| format!("create systemd template directory {}", output.display()))?;
    } else if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create launchd template directory {}", parent.display()))?;
    }

    let mut paths = Vec::with_capacity(files.len());
    for file in files {
        let path = if matches!(format, Format::Systemd) {
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
            let label = launchd_label(unit);
            format!(
                "mkdir -p \"$HOME/Library/LaunchAgents\" \"$HOME/Library/Logs/chat-stasher\" && cp {} \"$HOME/Library/LaunchAgents/{label}.plist\" && launchctl bootstrap \"gui/$(id -u)\" \"$HOME/Library/LaunchAgents/{label}.plist\"",
                paths
                    .first()
                    .map(|p| shell_quote(p))
                    .unwrap_or_else(|| "<generated-plist>".to_string())
            )
        }
        Format::Systemd => {
            let service_name = systemd_service_name(unit);
            let timer_name = systemd_timer_name(unit);
            let service = paths
                .iter()
                .find(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy() == service_name)
                })
                .map(|p| shell_quote(p))
                .unwrap_or_else(|| "<generated-service>".to_string());
            let timer = paths
                .iter()
                .find(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy() == timer_name)
                })
                .map(|p| shell_quote(p))
                .unwrap_or_else(|| "<generated-timer>".to_string());
            format!(
                "install -Dm644 {service} \"$HOME/.config/systemd/user/{service_name}\" && install -Dm644 {timer} \"$HOME/.config/systemd/user/{timer_name}\" && systemctl --user daemon-reload && systemctl --user enable --now {timer_name}"
            )
        }
    }
}

pub fn install_command_for_saved(unit: Unit, format: Format) -> String {
    match format {
        Format::Launchd => {
            let label = launchd_label(unit);
            format!(
                "mkdir -p \"$HOME/Library/LaunchAgents\" \"$HOME/Library/Logs/chat-stasher\" && launchctl bootstrap \"gui/$(id -u)\" \"$HOME/Library/LaunchAgents/{label}.plist\""
            )
        }
        Format::Systemd => {
            let timer_name = systemd_timer_name(unit);
            format!("systemctl --user daemon-reload && systemctl --user enable --now {timer_name}")
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
    if args.no_reap {
        argv.push("--no-reap".to_string());
    }
    argv
}

/// Build the `reap-stage` argument vector that both launchd and systemd embed.
/// The weekly timer is *always* an apply: a timer that only dry-ran would
/// report forever and delete nothing. Paths and values are kept as separate
/// tokens so each renderer can quote them in its own dialect.
fn reap_stage_argv(binary: &Path, stage: &Path, args: &ReapStageArgs) -> Vec<String> {
    let mut argv = vec![
        binary.to_string_lossy().into_owned(),
        "reap-stage".to_string(),
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
    if args.no_reap {
        argv.push("--no-reap".to_string());
    }
    argv
}

fn render_launchd(
    binary: &Path,
    stage: &Path,
    interval: u64,
    args: &RunOnceArgs,
    home: &Path,
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
  <!-- chat-stasher is a one-shot process: exit 0 is success; result=NOOP means no snapshot, result=COMPLETED means snapshot created; non-zero is error. -->
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
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
    )
}

fn render_launchd_reap_stage(
    binary: &Path,
    stage: &Path,
    args: &ReapStageArgs,
    home: &Path,
) -> String {
    let argv = reap_stage_argv(binary, stage, args);

    let log_dir = home.join("Library/Logs/chat-stasher");
    let stdout_log = log_dir.join("reap-stage.log");
    let stderr_log = log_dir.join("reap-stage.err.log");

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
  <!-- chat-stasher reap-stage is a weekly one-shot: exit 0 is success (reclaimed, or nothing to reclaim); exit 1 means BLOCKED (a destination could not be proven) and nothing was deleted; non-zero is error. -->
  <key>Label</key>
  <string>{LAUNCHD_LABEL_REAP_STAGE}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}
  </array>
  <!-- Sunday 03:17 local; :17 dodges the :00/:15/:30/:45 minutes cron-style jobs cluster on (launchd has no StartCalendarInterval jitter of its own, so the off-boundary minute is the stagger lever). See REAP_STAGE_HOUR / REAP_STAGE_MINUTE. -->
  <key>StartCalendarInterval</key>
  <dict>
    <key>Weekday</key>
    <integer>{REAP_STAGE_WEEKDAY}</integer>
    <key>Hour</key>
    <integer>{REAP_STAGE_HOUR}</integer>
    <key>Minute</key>
    <integer>{REAP_STAGE_MINUTE}</integer>
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

fn render_systemd_service(binary: &Path, stage: &Path, args: &RunOnceArgs) -> String {
    let argv = run_once_argv(binary, stage, args);
    let args = argv
        .iter()
        .map(|arg| systemd_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"[Unit]
Description=Run one chat-stasher archive cycle

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

fn render_systemd_timer(interval: u64) -> String {
    format!(
        r#"[Unit]
Description=Hourly chat-stasher archive cycle

[Timer]
OnBootSec={interval}s
OnUnitActiveSec={interval}s
Persistent=true
Unit={SYSTEMD_SERVICE}

[Install]
WantedBy=timers.target
"#
    )
}

fn render_systemd_service_reap_stage(binary: &Path, stage: &Path, args: &ReapStageArgs) -> String {
    let argv = reap_stage_argv(binary, stage, args);
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

fn render_systemd_timer_reap_stage() -> String {
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
Unit={SYSTEMD_SERVICE_REAP_STAGE}

[Install]
WantedBy=timers.target
"#,
        delay = REAP_STAGE_RANDOMIZED_DELAY_SECS,
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
            &ReapStageArgs::default(),
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
            &ReapStageArgs::default(),
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
            &ReapStageArgs::default(),
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
            &ReapStageArgs::default(),
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

        let systemd = render(
            Unit::RunOnce,
            Format::Systemd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            &args,
            &ReapStageArgs::default(),
            Path::new("/home/tester"),
        );
        let service = &systemd[0].content;
        assert!(service.contains("ExecStart="));
        assert!(service.contains("\"--destination\""));
        assert!(service.contains("\"external-disk\""));
        assert!(service.contains("\"--verify\""));
        assert!(service.contains("\"--connections\""));
        assert!(service.contains("\"2\""));
    }

    /// The weekly unit must embed the `reap-stage` subcommand, the stage path
    /// and `--apply` in *both* formats. This is the regression guard for the
    /// class of bug that once shipped `schedule` without `--destination`:
    /// string-containment alone missed it, so this test also checks the
    /// machine-facing shape of each renderer (calendar keys for launchd,
    /// unit names + OnCalendar for systemd).
    #[test]
    fn reap_stage_templates_embed_stage_and_subcommand_in_both_formats() {
        let reap_args = ReapStageArgs {
            repo: Some("backup-repo".to_string()),
            connections: Some(3),
            ..ReapStageArgs::default()
        };

        let launchd = render(
            Unit::ReapStage,
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            0,
            &RunOnceArgs::default(),
            &reap_args,
            Path::new("/home/tester"),
        );
        assert_eq!(launchd.len(), 1);
        assert_eq!(launchd[0].name, "com.chat-stasher.reap-stage.plist");
        let plist = &launchd[0].content;
        assert!(plist.contains("&apos;reap-stage&apos;"));
        assert!(plist.contains("&apos;/var/lib/chat-stasher/stage&apos;"));
        assert!(plist.contains("&apos;--apply&apos;"));
        assert!(plist.contains("&apos;--repo&apos;"));
        assert!(plist.contains("&apos;backup-repo&apos;"));
        // Logs sit next to the run-once logs but in separate files.
        assert!(plist.contains("reap-stage.log"));
        assert!(plist.contains("reap-stage.err.log"));
        assert!(!plist.contains("run-once.log"));
        // The weekly slot is a fixed calendar interval, never StartInterval.
        assert!(plist.contains("<key>StartCalendarInterval</key>"));
        assert!(!plist.contains("<key>StartInterval</key>"));
        assert!(plist.contains("<integer>0</integer>")); // Weekday = Sunday
        assert!(plist.contains("<integer>3</integer>")); // Hour
        assert!(plist.contains("<integer>17</integer>")); // Minute

        let systemd = render(
            Unit::ReapStage,
            Format::Systemd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            0,
            &RunOnceArgs::default(),
            &reap_args,
            Path::new("/home/tester"),
        );
        assert_eq!(systemd.len(), 2);
        assert_eq!(systemd[0].name, "chat-stasher-reap-stage.service");
        assert_eq!(systemd[1].name, "chat-stasher-reap-stage.timer");
        let service = &systemd[0].content;
        assert!(service.contains("ExecStart="));
        assert!(service.contains("\"reap-stage\""));
        assert!(service.contains("\"/var/lib/chat-stasher/stage\""));
        assert!(service.contains("\"--apply\""));
        assert!(service.contains("\"--repo\""));
        assert!(service.contains("\"backup-repo\""));
        let timer = &systemd[1].content;
        assert!(timer.contains("OnCalendar=Sun *-*-* 03:17:00"));
        assert!(timer.contains("RandomizedDelaySec=900s"));
        assert!(timer.contains("Unit=chat-stasher-reap-stage.service"));
        assert!(!timer.contains("chat-stasher-run-once.timer"));
    }

    #[test]
    fn reap_stage_unit_names_do_not_collide_with_run_once() {
        assert_ne!(launchd_label(Unit::RunOnce), launchd_label(Unit::ReapStage));
        assert_ne!(
            systemd_service_name(Unit::RunOnce),
            systemd_service_name(Unit::ReapStage)
        );
        assert_ne!(
            systemd_timer_name(Unit::RunOnce),
            systemd_timer_name(Unit::ReapStage)
        );
        // The install commands must target the unit they belong to.
        let run = install_command_for_saved(Unit::RunOnce, Format::Systemd);
        let reap = install_command_for_saved(Unit::ReapStage, Format::Systemd);
        assert!(run.contains("chat-stasher-run-once.timer"));
        assert!(!run.contains("chat-stasher-reap-stage.timer"));
        assert!(reap.contains("chat-stasher-reap-stage.timer"));
        assert!(!reap.contains("chat-stasher-run-once.timer"));
    }
}
