//! The parts of Discord's gateway protocol the bot uses, as pure functions.
//!
//! The bot keeps one WebSocket to the gateway: Discord says `Hello` (opcode
//! 10) with a heartbeat interval; the bot identifies (2) and sends a
//! heartbeat (1) every interval, which Discord acknowledges (11). Events
//! arrive as dispatches (0), e.g. `READY` and `MESSAGE_CREATE`. Discord may
//! ask for an immediate heartbeat (1), a reconnect (7), or invalidate the
//! session (9).
//! <https://discord.com/developers/docs/events/gateway>

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::parse::ParseError;

/// GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT. Message content is a
/// privileged intent: enable it for the bot in the developer portal.
pub const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);

/// Discord's epoch (2015-01-01), in Unix milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Close codes after which reconnecting cannot help: authentication failed,
/// invalid or disallowed intents.
pub const FATAL_CLOSE_CODES: [u16; 4] = [4004, 4012, 4013, 4014];

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Hello {
        heartbeat_interval: Duration,
    },
    Dispatch {
        seq: Option<u64>,
        kind: String,
        data: Value,
    },
    /// Discord asks for a heartbeat now.
    HeartbeatRequest,
    HeartbeatAck,
    Reconnect,
    InvalidSession,
    /// An opcode the bot does not use.
    Other(u64),
}

#[derive(Debug, Deserialize)]
struct Frame {
    op: u64,
    #[serde(default)]
    d: Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

pub fn parse_frame(text: &str) -> Result<Incoming, ParseError> {
    let frame: Frame = serde_json::from_str(text).map_err(|e| ParseError::Json(e.to_string()))?;
    Ok(match frame.op {
        0 => Incoming::Dispatch {
            seq: frame.s,
            kind: frame.t.unwrap_or_default(),
            data: frame.d,
        },
        1 => Incoming::HeartbeatRequest,
        7 => Incoming::Reconnect,
        9 => Incoming::InvalidSession,
        10 => {
            let ms = frame
                .d
                .get("heartbeat_interval")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
                .ok_or(ParseError::Field {
                    field: "heartbeat_interval",
                    problem: "is missing or not a positive number",
                })?;
            Incoming::Hello {
                heartbeat_interval: Duration::from_millis(ms),
            }
        }
        11 => Incoming::HeartbeatAck,
        other => Incoming::Other(other),
    })
}

pub fn identify(token: &str) -> String {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": INTENTS,
            "properties": { "os": "linux", "browser": "homeguard", "device": "homeguard" }
        }
    })
    .to_string()
}

/// A heartbeat carries the last sequence number received.
pub fn heartbeat(seq: Option<u64>) -> String {
    json!({ "op": 1, "d": seq }).to_string()
}

/// The fields of a `MESSAGE_CREATE` the bot needs. Ids are snowflakes,
/// sent as strings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageCreate {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    pub author: Author,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Author {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub bot: bool,
}

/// The creation time of a snowflake id, in Unix milliseconds.
pub fn snowflake_unix_ms(id: u64) -> u64 {
    (id >> 22) + DISCORD_EPOCH_MS
}

/// The snowflake an id created at `unix_ms` would have (for tests and
/// fakes).
pub fn snowflake_at(unix_ms: u64) -> u64 {
    unix_ms.saturating_sub(DISCORD_EPOCH_MS) << 22
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// From a bot (including this one): never answered, not logged.
    IgnoreBot,
    /// Not from an allowed user in an allowed channel: ignored and logged.
    Denied,
    /// Older than the maximum command age.
    Stale,
    Allowed,
}

/// Who may command the bot: an allowed user, in an allowed channel when a
/// channel list is configured (direct messages count as channels too), with
/// a message no older than `max_age_ms`.
pub fn authorize(
    message: &MessageCreate,
    users: &[u64],
    channels: &[u64],
    now_unix_ms: u64,
    max_age_ms: u64,
) -> Decision {
    if message.author.bot {
        return Decision::IgnoreBot;
    }
    let user_ok = message
        .author
        .id
        .parse::<u64>()
        .is_ok_and(|id| users.contains(&id));
    let channel_ok = channels.is_empty()
        || message
            .channel_id
            .parse::<u64>()
            .is_ok_and(|id| channels.contains(&id));
    if !user_ok || !channel_ok {
        return Decision::Denied;
    }
    let created = message
        .id
        .parse::<u64>()
        .map(snowflake_unix_ms)
        .unwrap_or(0);
    if now_unix_ms.saturating_sub(created) > max_age_ms {
        return Decision::Stale;
    }
    Decision::Allowed
}

/// Message content is limited to 2000 characters.
pub fn fit(text: &str) -> String {
    const MAX: usize = 2000;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames() {
        assert_eq!(
            parse_frame(r#"{"op":10,"d":{"heartbeat_interval":41250},"s":null,"t":null}"#),
            Ok(Incoming::Hello {
                heartbeat_interval: Duration::from_millis(41_250)
            })
        );
        assert_eq!(parse_frame(r#"{"op":11}"#), Ok(Incoming::HeartbeatAck));
        assert_eq!(
            parse_frame(r#"{"op":1,"d":null}"#),
            Ok(Incoming::HeartbeatRequest)
        );
        assert_eq!(parse_frame(r#"{"op":7,"d":null}"#), Ok(Incoming::Reconnect));
        assert_eq!(
            parse_frame(r#"{"op":9,"d":false}"#),
            Ok(Incoming::InvalidSession)
        );
        assert_eq!(parse_frame(r#"{"op":3}"#), Ok(Incoming::Other(3)));
        let dispatch = parse_frame(r#"{"op":0,"s":42,"t":"MESSAGE_CREATE","d":{"x":1}}"#).unwrap();
        assert_eq!(
            dispatch,
            Incoming::Dispatch {
                seq: Some(42),
                kind: "MESSAGE_CREATE".into(),
                data: json!({"x": 1})
            }
        );
        assert!(parse_frame("not json").is_err());
        assert!(parse_frame(r#"{"op":10,"d":{}}"#).is_err());
        assert!(parse_frame(r#"{"d":{}}"#).is_err());
    }

    #[test]
    fn builds_identify_and_heartbeat() {
        let id: Value = serde_json::from_str(&identify("tok")).unwrap();
        assert_eq!(id["op"], 2);
        assert_eq!(id["d"]["token"], "tok");
        assert_eq!(id["d"]["intents"], 37_376);
        assert_eq!(heartbeat(None), r#"{"d":null,"op":1}"#);
        assert_eq!(heartbeat(Some(7)), r#"{"d":7,"op":1}"#);
    }

    #[test]
    fn snowflake_times() {
        // The example from Discord's documentation.
        assert_eq!(
            snowflake_unix_ms(175_928_847_299_117_063),
            1_462_015_105_796
        );
        let t = 1_793_051_000_000;
        assert_eq!(snowflake_unix_ms(snowflake_at(t)), t);
    }

    fn message(author: &str, channel: &str, bot: bool, created_ms: u64) -> MessageCreate {
        MessageCreate {
            id: snowflake_at(created_ms).to_string(),
            channel_id: channel.into(),
            guild_id: None,
            author: Author {
                id: author.into(),
                username: "someone".into(),
                bot,
            },
            content: "!status".into(),
        }
    }

    #[test]
    fn authorization() {
        let now = 1_793_051_000_000;
        let max = 120_000;
        let users = [111_u64];
        let channels = [900_u64];
        let check = |m: &MessageCreate, channels: &[u64]| authorize(m, &users, channels, now, max);
        assert_eq!(
            check(&message("111", "900", false, now), &channels),
            Decision::Allowed
        );
        assert_eq!(
            check(&message("111", "901", false, now), &[]),
            Decision::Allowed
        );
        assert_eq!(
            check(&message("222", "900", false, now), &channels),
            Decision::Denied
        );
        assert_eq!(
            check(&message("111", "901", false, now), &channels),
            Decision::Denied
        );
        assert_eq!(
            check(&message("111", "900", true, now), &channels),
            Decision::IgnoreBot
        );
        assert_eq!(
            check(&message("x", "900", false, now), &channels),
            Decision::Denied
        );
        assert_eq!(
            check(&message("111", "900", false, now - 120_001), &channels),
            Decision::Stale
        );
        assert_eq!(
            check(&message("111", "900", false, now - 120_000), &channels),
            Decision::Allowed
        );
    }

    #[test]
    fn decodes_message_create() {
        let data = json!({
            "id": "1234", "channel_id": "900", "guild_id": "55", "type": 0,
            "author": {"id": "111", "username": "alice", "global_name": "Alice"},
            "content": "!arm away", "mentions": [], "tts": false
        });
        let m: MessageCreate = serde_json::from_value(data).unwrap();
        assert_eq!(m.author.username, "alice");
        assert!(!m.author.bot);
        assert_eq!(m.content, "!arm away");
    }

    #[test]
    fn long_replies_are_cut() {
        assert_eq!(fit("short"), "short");
        let long = "é".repeat(2500);
        let cut = fit(&long);
        assert_eq!(cut.chars().count(), 2000);
        assert!(cut.ends_with('…'));
    }
}
