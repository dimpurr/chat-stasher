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

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Format {
    /// macOS launchd property list.
    Launchd,
    /// Linux systemd user service + timer.
    Systemd,
}

#[derive(Debug, Clone)]
pub struct TemplateFile {
    pub name: String,
    pub content: String,
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

pub fn render(
    format: Format,
    binary: &Path,
    stage: &Path,
    interval: u64,
    verify: bool,
    home: &Path,
) -> Vec<TemplateFile> {
    match format {
        Format::Launchd => vec![TemplateFile {
            name: format!("{LAUNCHD_LABEL}.plist"),
            content: render_launchd(binary, stage, interval, verify, home),
        }],
        Format::Systemd => vec![
            TemplateFile {
                name: SYSTEMD_SERVICE.to_string(),
                content: render_systemd_service(binary, stage, verify),
            },
            TemplateFile {
                name: SYSTEMD_TIMER.to_string(),
                content: render_systemd_timer(interval),
            },
        ],
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

pub fn install_command(format: Format, paths: &[PathBuf]) -> String {
    match format {
        Format::Launchd => format!(
            "mkdir -p \"$HOME/Library/LaunchAgents\" \"$HOME/Library/Logs/chat-stasher\" && cp {} \"$HOME/Library/LaunchAgents/{LAUNCHD_LABEL}.plist\" && launchctl bootstrap \"gui/$(id -u)\" \"$HOME/Library/LaunchAgents/{LAUNCHD_LABEL}.plist\"",
            paths
                .first()
                .map(|p| shell_quote(p))
                .unwrap_or_else(|| "<generated-plist>".to_string())
        ),
        Format::Systemd => {
            let service = paths
                .iter()
                .find(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy() == SYSTEMD_SERVICE)
                })
                .map(|p| shell_quote(p))
                .unwrap_or_else(|| "<generated-service>".to_string());
            let timer = paths
                .iter()
                .find(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy() == SYSTEMD_TIMER)
                })
                .map(|p| shell_quote(p))
                .unwrap_or_else(|| "<generated-timer>".to_string());
            format!(
                "install -Dm644 {service} \"$HOME/.config/systemd/user/{SYSTEMD_SERVICE}\" && install -Dm644 {timer} \"$HOME/.config/systemd/user/{SYSTEMD_TIMER}\" && systemctl --user daemon-reload && systemctl --user enable --now {SYSTEMD_TIMER}"
            )
        }
    }
}

pub fn install_command_for_saved(format: Format) -> String {
    match format {
        Format::Launchd => format!(
            "mkdir -p \"$HOME/Library/LaunchAgents\" \"$HOME/Library/Logs/chat-stasher\" && launchctl bootstrap \"gui/$(id -u)\" \"$HOME/Library/LaunchAgents/{LAUNCHD_LABEL}.plist\""
        ),
        Format::Systemd => format!(
            "systemctl --user daemon-reload && systemctl --user enable --now {SYSTEMD_TIMER}"
        ),
    }
}

fn render_launchd(binary: &Path, stage: &Path, interval: u64, verify: bool, home: &Path) -> String {
    let mut args = vec![
        binary.to_string_lossy().into_owned(),
        "run-once".to_string(),
        "--stage".to_string(),
        stage.to_string_lossy().into_owned(),
    ];
    if verify {
        args.push("--verify".to_string());
    }
    let arguments = args
        .iter()
        .map(|arg| format!("        <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    let log_dir = home.join("Library/Logs/chat-stasher");
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
        stdout = xml_escape(&log_dir.join("run-once.log").to_string_lossy()),
        stderr = xml_escape(&log_dir.join("run-once.err.log").to_string_lossy()),
    )
}

fn render_systemd_service(binary: &Path, stage: &Path, verify: bool) -> String {
    let mut args = format!(
        "{} run-once --stage {}",
        systemd_quote(binary),
        systemd_quote(stage)
    );
    if verify {
        args.push_str(" --verify");
    }
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

fn systemd_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
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
    fn scheduler_templates_describe_zero_as_the_only_success_status() {
        let files = render(
            Format::Systemd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            false,
            Path::new("/home/tester"),
        );
        assert_eq!(files.len(), 2);
        assert!(files[0].content.contains("SuccessExitStatus=0\n"));
        assert!(!files[0].content.contains("SuccessExitStatus=0 10"));
        assert!(files[0].content.contains("result=NOOP means no snapshot"));
        assert!(files[1].content.contains("OnUnitActiveSec=3600s"));
        let launchd = render(
            Format::Launchd,
            Path::new("/opt/chat-stasher"),
            Path::new("/var/lib/chat-stasher/stage"),
            3600,
            false,
            Path::new("/home/tester"),
        );
        assert!(launchd[0]
            .content
            .contains("exit 0 is success; result=NOOP means no snapshot"));
    }
}
