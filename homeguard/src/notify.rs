//! What the chat bots (Telegram, Discord) have in common: the alert sent
//! to them, their commands, authorization and message texts. Pure.
//!
//! Commands are the same on every bot; only the prefix differs (`/status`
//! on Telegram, `!status` on Discord).

use std::str::FromStr;

use thiserror::Error;

use crate::core::{AlertLevel, Mode};
use crate::status::{LogLine, Status};

/// An alert for the delivery workers.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub level: AlertLevel,
    pub text: String,
    /// A Frigate event id whose snapshot to attach.
    pub snapshot_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    Status,
    Arm(Mode),
    Disarm,
    Snapshot(String),
    Events,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandError {
    #[error("not a command")]
    NotACommand,
    /// `/cmd@other_bot` in a Telegram group: meant for another bot, stay silent.
    #[error("addressed to another bot")]
    OtherBot,
    #[error("unknown command {0:?}")]
    Unknown(String),
    #[error("{command} needs {what}")]
    MissingArgument {
        command: &'static str,
        what: &'static str,
    },
    #[error("{0:?} is not a mode: use away, home or night")]
    BadMode(String),
}

/// Parses `/arm away`, `/arm@homeguard_bot night`, `!STATUS`, ... after
/// `prefix`. `bot_username` (without `@`) lets Telegram commands addressed
/// to other bots be ignored.
pub fn parse_command(
    text: &str,
    prefix: char,
    bot_username: Option<&str>,
) -> Result<BotCommand, CommandError> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix(prefix) else {
        return Err(CommandError::NotACommand);
    };
    let mut words = rest.split_whitespace();
    let head = words.next().unwrap_or_default();
    let (name, target) = match head.split_once('@') {
        Some((name, target)) => (name, Some(target)),
        None => (head, None),
    };
    if let (Some(target), Some(me)) = (target, bot_username) {
        if !target.eq_ignore_ascii_case(me) {
            return Err(CommandError::OtherBot);
        }
    }
    let arg = words.next();
    match name.to_ascii_lowercase().as_str() {
        "status" => Ok(BotCommand::Status),
        "arm" => {
            let mode = arg.ok_or(CommandError::MissingArgument {
                command: "arm",
                what: "a mode: away, home or night",
            })?;
            Mode::parse(mode)
                .map(BotCommand::Arm)
                .ok_or_else(|| CommandError::BadMode(mode.to_string()))
        }
        "disarm" => Ok(BotCommand::Disarm),
        "snapshot" => arg
            .map(|camera| BotCommand::Snapshot(camera.to_string()))
            .ok_or(CommandError::MissingArgument {
                command: "snapshot",
                what: "a camera name",
            }),
        "events" => Ok(BotCommand::Events),
        "help" | "start" => Ok(BotCommand::Help),
        other => Err(CommandError::Unknown(other.to_string())),
    }
}

/// Parses an allow-list of ids (Telegram chat ids, Discord user or channel
/// ids) separated by commas or spaces. `what` names them in errors.
pub fn parse_ids<T: FromStr>(text: &str, what: &str) -> Result<Vec<T>, String> {
    let ids: Result<Vec<T>, String> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<T>()
                .map_err(|_| format!("{s:?} is not a {what} (a number such as 123456789)"))
        })
        .collect();
    let ids = ids?;
    if ids.is_empty() {
        return Err(format!("no {what} given"));
    }
    Ok(ids)
}

/// Only allow-listed ids may command a bot.
pub fn is_allowed<T: PartialEq>(id: T, allowed: &[T]) -> bool {
    allowed.contains(&id)
}

/// The command list, with the bot's prefix.
pub fn help(prefix: char) -> String {
    format!(
        "homeguard commands:
{prefix}status - arm state, devices offline, who is home
{prefix}arm away|home|night - arm (after the exit delay)
{prefix}disarm - disarm
{prefix}snapshot <camera> - a picture from a camera
{prefix}events - the last 10 events"
    )
}

/// An alert as sent: a marker for the level, then the text.
pub fn format_alert(level: AlertLevel, text: &str) -> String {
    let marker = match level {
        AlertLevel::Critical => "🚨",
        AlertLevel::High => "⚠️",
        AlertLevel::Info => "ℹ️",
        AlertLevel::Low => "·",
    };
    format!("{marker} {text}")
}

/// Low-priority alerts are delivered without a sound.
pub fn silent(level: AlertLevel) -> bool {
    level == AlertLevel::Low
}

pub fn format_status(status: &Status) -> String {
    let mut out = match status.mode {
        Some(mode) => format!("State: {} ({mode})", status.state),
        None => format!("State: {}", status.state),
    };
    if let Some(cause) = &status.cause {
        out.push_str(&format!("\nCause: {cause}"));
    }
    let devices: Vec<_> = status.zones.iter().flat_map(|z| &z.devices).collect();
    let offline: Vec<&str> = devices
        .iter()
        .filter(|d| !d.online)
        .map(|d| d.name.as_str())
        .collect();
    let open: Vec<&str> = devices
        .iter()
        .filter(|d| d.open == Some(true))
        .map(|d| d.name.as_str())
        .collect();
    out.push_str(&format!(
        "\nOffline: {}",
        if offline.is_empty() {
            "none".to_string()
        } else {
            offline.join(", ")
        }
    ));
    if !open.is_empty() {
        out.push_str(&format!("\nOpen: {}", open.join(", ")));
    }
    if !status.presence.is_empty() {
        let people: Vec<String> = status
            .presence
            .iter()
            .map(|p| {
                let where_ = match p.home {
                    Some(true) => "home",
                    Some(false) => "away",
                    None => "unknown",
                };
                format!("{} {where_}", p.name)
            })
            .collect();
        out.push_str(&format!("\nPeople: {}", people.join(", ")));
    }
    out
}

pub fn format_events(lines: &[LogLine], now_unix_ms: u64) -> String {
    if lines.is_empty() {
        return "No events yet.".into();
    }
    lines
        .iter()
        .map(|l| {
            format!(
                "{} ago: {}",
                ago(now_unix_ms.saturating_sub(l.unix_ms)),
                l.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d", s / 86_400),
    }
}

/// A Frigate event id or camera name safe to put in a URL path.
pub fn safe_path_segment(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && text != "."
        && text != ".."
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{DeviceStatus, PersonStatus, ZoneStatus};

    #[test]
    fn parses_commands() {
        let p = |t| parse_command(t, '/', Some("homeguard_bot"));
        assert_eq!(p("/status"), Ok(BotCommand::Status));
        assert_eq!(p("  /STATUS  "), Ok(BotCommand::Status));
        assert_eq!(p("/arm away"), Ok(BotCommand::Arm(Mode::Away)));
        assert_eq!(p("/arm   Night"), Ok(BotCommand::Arm(Mode::Night)));
        assert_eq!(
            p("/arm@homeguard_bot home"),
            Ok(BotCommand::Arm(Mode::Home))
        );
        assert_eq!(
            p("/arm@HomeGuard_Bot home"),
            Ok(BotCommand::Arm(Mode::Home))
        );
        assert_eq!(p("/disarm"), Ok(BotCommand::Disarm));
        assert_eq!(
            p("/snapshot front_cam"),
            Ok(BotCommand::Snapshot("front_cam".into()))
        );
        assert_eq!(p("/events"), Ok(BotCommand::Events));
        assert_eq!(p("/start"), Ok(BotCommand::Help));
        assert_eq!(p("/help"), Ok(BotCommand::Help));
    }

    #[test]
    fn rejects_bad_commands() {
        let p = |t| parse_command(t, '/', Some("homeguard_bot"));
        assert_eq!(p("hello"), Err(CommandError::NotACommand));
        assert_eq!(p(""), Err(CommandError::NotACommand));
        assert_eq!(p("/arm@other_bot away"), Err(CommandError::OtherBot));
        assert_eq!(p("/reboot"), Err(CommandError::Unknown("reboot".into())));
        assert!(matches!(
            p("/arm"),
            Err(CommandError::MissingArgument { .. })
        ));
        assert_eq!(
            p("/arm vacation"),
            Err(CommandError::BadMode("vacation".into()))
        );
        assert!(matches!(
            p("/snapshot"),
            Err(CommandError::MissingArgument { .. })
        ));
        assert_eq!(p("/"), Err(CommandError::Unknown(String::new())));
        // Without knowing our own name, addressed commands are accepted.
        assert_eq!(
            parse_command("/status@x", '/', None),
            Ok(BotCommand::Status)
        );
        // Another prefix: Discord.
        assert_eq!(
            parse_command("!arm Night", '!', None),
            Ok(BotCommand::Arm(Mode::Night))
        );
        assert_eq!(
            parse_command("/status", '!', None),
            Err(CommandError::NotACommand)
        );
    }

    #[test]
    fn authorization() {
        let allowed: Vec<i64> = parse_ids("123456789, -1001234567890 42", "chat id").unwrap();
        assert_eq!(allowed, vec![123_456_789, -1_001_234_567_890, 42]);
        assert!(is_allowed(42, &allowed));
        assert!(is_allowed(-1_001_234_567_890, &allowed));
        assert!(!is_allowed(43, &allowed));
        assert!(!is_allowed(0_i64, &[]));
        assert!(parse_ids::<i64>("", "chat id").is_err());
        assert!(parse_ids::<i64>("12, abc", "chat id").is_err());
        // Discord snowflakes are unsigned 64-bit.
        let users: Vec<u64> = parse_ids("80351110224678912", "user id").unwrap();
        assert!(is_allowed(80_351_110_224_678_912, &users));
        assert!(parse_ids::<u64>("-5", "user id").is_err());
    }

    #[test]
    fn formats_status_and_events() {
        let status = Status {
            state: "pending".into(),
            mode: Some(Mode::Away),
            cause: Some("Front door opened".into()),
            snapshot_ref: None,
            zones: vec![ZoneStatus {
                id: "hall".into(),
                devices: vec![
                    DeviceStatus {
                        id: "door".into(),
                        name: "Front door".into(),
                        kind: "contact",
                        online: true,
                        open: Some(true),
                    },
                    DeviceStatus {
                        id: "cam".into(),
                        name: "Hall camera".into(),
                        kind: "camera",
                        online: false,
                        open: None,
                    },
                ],
            }],
            presence: vec![PersonStatus {
                name: "alice".into(),
                home: Some(false),
            }],
        };
        assert_eq!(
            format_status(&status),
            "State: pending (away)\nCause: Front door opened\nOffline: Hall camera\n\
             Open: Front door\nPeople: alice away"
        );
        let lines = vec![LogLine {
            unix_ms: 1_000_000,
            kind: "alert",
            level: Some(AlertLevel::High),
            text: "Entry".into(),
            snapshot_ref: None,
        }];
        assert_eq!(format_events(&lines, 1_000_000 + 125_000), "2m ago: Entry");
        assert_eq!(format_events(&[], 0), "No events yet.");
        assert_eq!(ago(3_725_000), "1h02m");
        assert_eq!(ago(200_000_000), "2d");
    }

    #[test]
    fn alerts_and_path_segments() {
        assert_eq!(format_alert(AlertLevel::Critical, "ALARM"), "🚨 ALARM");
        assert!(silent(AlertLevel::Low));
        assert!(!silent(AlertLevel::Critical));
        assert!(safe_path_segment("1793051000.123456-k3x9qa"));
        assert!(safe_path_segment("front_cam"));
        for bad in ["", "..", "../etc", "a/b", "a?b=c", "a b"] {
            assert!(!safe_path_segment(bad), "{bad}");
        }
    }
}
