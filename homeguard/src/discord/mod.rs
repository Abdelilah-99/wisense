//! Discord: alerts through a webhook, commands through a bot.
//!
//! - **Alerts** go to a channel webhook (`DISCORD_WEBHOOK_URL`), with the
//!   Frigate snapshot attached when there is one. Without a webhook, a
//!   configured bot posts them to its allowed channels instead.
//! - **Commands** (`!status`, `!arm away`, ...) are read by a bot over the
//!   gateway WebSocket. The connection is outbound, like Telegram's
//!   polling, so nothing on the hub is exposed. Only allow-listed users
//!   (and, if configured, channels) are answered.
//!
//! Webhook URLs and bot tokens are secrets: errors are stripped of URLs
//! before they are logged, and the token only travels in a header.

pub mod gateway;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::chat::{execute, Reply};
use crate::core::time::serde_duration;
use crate::delivery::{scrub, with_retry, ApiError};
use crate::frigate::FrigateClient;
use crate::mqtt::Backoff;
use crate::notify::{format_alert, help, parse_command, parse_ids, silent, Alert, CommandError};
use crate::run::{unix_ms, CoreHandle};
use gateway::{authorize, fit, parse_frame, Decision, Incoming, MessageCreate};

/// Discord's SUPPRESS_NOTIFICATIONS message flag: delivered silently.
const SUPPRESS_NOTIFICATIONS: u64 = 1 << 12;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiscordConfig {
    /// Environment variable holding the channel webhook URL (alerts).
    pub webhook_env: String,
    /// Environment variable holding the bot token (commands).
    pub token_env: String,
    /// Environment variable holding the user ids allowed to send commands.
    pub users_env: String,
    /// Environment variable holding the channel ids where commands are
    /// accepted (optional: empty means any channel the bot can read, and
    /// direct messages).
    pub channels_env: String,
    pub prefix: char,
    pub api_url: String,
    pub gateway_url: String,
    /// Commands older than this (sent while homeguard was down) are ignored.
    #[serde(with = "serde_duration")]
    pub max_command_age: Duration,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        DiscordConfig {
            webhook_env: "DISCORD_WEBHOOK_URL".into(),
            token_env: "DISCORD_BOT_TOKEN".into(),
            users_env: "DISCORD_ALLOWED_USER_IDS".into(),
            channels_env: "DISCORD_ALLOWED_CHANNEL_IDS".into(),
            prefix: '!',
            api_url: "https://discord.com/api/v10".into(),
            gateway_url: "wss://gateway.discord.gg/?v=10&encoding=json".into(),
            max_command_age: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BotCredentials {
    pub token: String,
    pub users: Vec<u64>,
    pub channels: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub webhook: Option<String>,
    pub bot: Option<BotCredentials>,
}

impl DiscordConfig {
    /// What is configured in the environment. Nothing set means Discord is
    /// off; a token without allowed users is an error (nobody could use it).
    pub fn credentials(&self, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Credentials> {
        let get = |name: &str| {
            env(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let webhook = get(&self.webhook_env);
        if let Some(url) = &webhook {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                anyhow::bail!("{} must be the webhook's full URL", self.webhook_env);
            }
        }
        let bot = match get(&self.token_env) {
            None => None,
            Some(token) => {
                let users = get(&self.users_env).unwrap_or_default();
                let users = parse_ids::<u64>(&users, "Discord user id")
                    .map_err(|e| anyhow::anyhow!("{}: {e}", self.users_env))?;
                let channels = match get(&self.channels_env) {
                    Some(text) => parse_ids::<u64>(&text, "Discord channel id")
                        .map_err(|e| anyhow::anyhow!("{}: {e}", self.channels_env))?,
                    None => Vec::new(),
                };
                Some(BotCredentials {
                    token,
                    users,
                    channels,
                })
            }
        };
        Ok(Credentials { webhook, bot })
    }
}

/// Discord's REST API: webhook executions and bot messages.
#[derive(Clone)]
pub struct Rest {
    http: reqwest::Client,
    api: String,
    /// `Bot <token>`, when a bot is configured.
    authorization: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RateLimited {
    retry_after: Option<f64>,
}

impl Rest {
    pub fn new(config: &DiscordConfig, token: Option<&str>) -> anyhow::Result<Rest> {
        Ok(Rest {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| anyhow::anyhow!("HTTP client: {}", scrub(e)))?,
            api: config.api_url.trim_end_matches('/').to_string(),
            authorization: token.map(|t| format!("Bot {t}")),
        })
    }

    /// The message body: text, silent flag, and no pings ever.
    fn payload(text: &str, quiet: bool) -> serde_json::Value {
        json!({
            "content": fit(text),
            "flags": if quiet { SUPPRESS_NOTIFICATIONS } else { 0 },
            "allowed_mentions": { "parse": [] },
        })
    }

    async fn post(
        &self,
        what: &str,
        request: reqwest::RequestBuilder,
        text: &str,
        photo: Option<Vec<u8>>,
        quiet: bool,
    ) -> Result<(), ApiError> {
        let payload = Rest::payload(text, quiet);
        let request = match photo {
            None => request.json(&payload),
            Some(jpeg) => {
                let part = Part::bytes(jpeg)
                    .file_name("snapshot.jpg")
                    .mime_str("image/jpeg")
                    .map_err(|e| ApiError::Fatal(scrub(e)))?;
                let form = Form::new()
                    .text("payload_json", payload.to_string())
                    .part("files[0]", part);
                request.multipart(form)
            }
        };
        let response = request.send().await.map_err(|e| ApiError::Retry {
            after: None,
            reason: format!("{what}: {}", scrub(e)),
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let header_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok());
        let body = response.text().await.unwrap_or_default();
        let body_after = serde_json::from_str::<RateLimited>(&body)
            .ok()
            .and_then(|r| r.retry_after);
        let after = body_after
            .or(header_after)
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(|s| Duration::from_secs_f64(s.min(300.0)));
        let detail: String = body.chars().take(200).collect();
        Err(ApiError::from_status(
            status.as_u16(),
            after,
            format!("{what}: {status} {detail}"),
        ))
    }

    /// Executes a webhook (posts as the webhook to its channel).
    pub async fn webhook(
        &self,
        url: &str,
        text: &str,
        photo: Option<Vec<u8>>,
        quiet: bool,
    ) -> Result<(), ApiError> {
        // wait=true: Discord confirms the message was created.
        let separator = if url.contains('?') { '&' } else { '?' };
        let request = self.http.post(format!("{url}{separator}wait=true"));
        self.post("webhook", request, text, photo, quiet).await
    }

    /// Posts as the bot in a channel (or a direct message channel).
    pub async fn create_message(
        &self,
        channel: &str,
        text: &str,
        photo: Option<Vec<u8>>,
        quiet: bool,
    ) -> Result<(), ApiError> {
        let Some(authorization) = &self.authorization else {
            return Err(ApiError::Fatal("no bot token".into()));
        };
        let request = self
            .http
            .post(format!("{}/channels/{channel}/messages", self.api))
            .header(reqwest::header::AUTHORIZATION, authorization);
        self.post("create message", request, text, photo, quiet)
            .await
    }
}

/// Where alerts go.
#[derive(Debug, Clone)]
pub enum AlertTarget {
    Webhook(String),
    /// The bot's allowed channels.
    Channels(Vec<u64>),
}

/// Sends every queued alert, with the snapshot when possible, falling back
/// to text, retrying transient failures.
pub async fn alert_worker(
    rest: Rest,
    target: AlertTarget,
    frigate: FrigateClient,
    mut alerts: mpsc::Receiver<Alert>,
) {
    while let Some(alert) = alerts.recv().await {
        let text = format_alert(alert.level, &alert.text);
        let quiet = silent(alert.level);
        let photo = match alert.snapshot_ref.as_deref() {
            Some(id) => match frigate.event_snapshot(id).await {
                Ok(photo) => Some(photo),
                Err(e) => {
                    warn!(error = %e, "Frigate snapshot unavailable; sending text only");
                    None
                }
            },
            None => None,
        };
        match &target {
            AlertTarget::Webhook(url) => {
                deliver(&rest, Destination::Webhook(url), &text, photo, quiet).await
            }
            AlertTarget::Channels(channels) => {
                for channel in channels {
                    let channel = channel.to_string();
                    deliver(
                        &rest,
                        Destination::Channel(&channel),
                        &text,
                        photo.clone(),
                        quiet,
                    )
                    .await;
                }
            }
        }
    }
}

enum Destination<'a> {
    Webhook(&'a str),
    Channel(&'a str),
}

async fn deliver(
    rest: &Rest,
    to: Destination<'_>,
    text: &str,
    photo: Option<Vec<u8>>,
    quiet: bool,
) {
    let send = |photo: Option<Vec<u8>>| async {
        match to {
            Destination::Webhook(url) => rest.webhook(url, text, photo, quiet).await,
            Destination::Channel(channel) => rest.create_message(channel, text, photo, quiet).await,
        }
    };
    if let Some(photo) = photo {
        match with_retry(|| send(Some(photo.clone()))).await {
            Ok(()) => return,
            Err(e) => warn!(error = %e, "sending the snapshot to Discord failed; sending text"),
        }
    }
    if let Err(e) = with_retry(|| send(None)).await {
        warn!(error = %e, "Discord message not delivered");
    }
}

/// How a gateway session ended.
enum SessionEnd {
    /// Reconnect (Discord asked, or the connection dropped).
    Reconnect,
    /// Reconnecting cannot help (bad token, intents not enabled).
    Fatal(String),
}

/// Keeps the bot connected forever (or until Discord refuses it for good),
/// answering allowed commands.
pub async fn bot_loop(
    config: DiscordConfig,
    credentials: BotCredentials,
    rest: Rest,
    core: CoreHandle,
    frigate: FrigateClient,
) {
    let mut backoff = Backoff::new(Duration::from_secs(120));
    loop {
        match session(&config, &credentials, &rest, &core, &frigate, &mut backoff).await {
            Ok(SessionEnd::Reconnect) => {}
            Ok(SessionEnd::Fatal(reason)) => {
                error!(%reason, "Discord bot stopped; fix the configuration and restart homeguard");
                return;
            }
            Err(e) => warn!(error = %format!("{e:#}"), "Discord gateway connection failed"),
        }
        let delay = backoff.next_delay();
        info!(retry_in = ?delay, "reconnecting to the Discord gateway");
        tokio::time::sleep(delay).await;
    }
}

async fn session(
    config: &DiscordConfig,
    credentials: &BotCredentials,
    rest: &Rest,
    core: &CoreHandle,
    frigate: &FrigateClient,
    backoff: &mut Backoff,
) -> anyhow::Result<SessionEnd> {
    let (socket, _) = tokio_tungstenite::connect_async(config.gateway_url.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("connecting: {e}"))?;
    let (mut sink, mut stream) = socket.split();

    let interval = match next_frame(&mut stream).await? {
        Some(Incoming::Hello { heartbeat_interval }) => heartbeat_interval,
        other => anyhow::bail!("expected Hello, got {other:?}"),
    };
    sink.send(Message::text(gateway::identify(&credentials.token)))
        .await?;

    // The first heartbeat comes after a fraction of the interval, as
    // Discord asks, so a fleet of clients does not beat in step.
    let first = tokio::time::Instant::now() + interval / 3;
    let mut ticker = tokio::time::interval_at(first, interval);
    let mut seq: Option<u64> = None;
    let mut acked = true;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if !acked {
                    warn!("Discord did not acknowledge the last heartbeat; reconnecting");
                    return Ok(SessionEnd::Reconnect);
                }
                acked = false;
                sink.send(Message::text(gateway::heartbeat(seq))).await?;
            }
            message = stream.next() => {
                let text = match message {
                    None => return Ok(SessionEnd::Reconnect),
                    Some(Err(e)) => anyhow::bail!("gateway: {e}"),
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                        let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                        if gateway::FATAL_CLOSE_CODES.contains(&code) {
                            return Ok(SessionEnd::Fatal(format!(
                                "Discord closed the connection ({code} {reason}): check \
                                 DISCORD_BOT_TOKEN and enable the Message Content intent"
                            )));
                        }
                        warn!(code, %reason, "Discord closed the gateway connection");
                        return Ok(SessionEnd::Reconnect);
                    }
                    Some(Ok(_)) => continue, // pings are answered by the library
                };
                match parse_frame(text.as_str()) {
                    Ok(Incoming::Dispatch { seq: s, kind, data }) => {
                        seq = s.or(seq);
                        match kind.as_str() {
                            "READY" => {
                                backoff.reset();
                                let name = data
                                    .pointer("/user/username")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                info!(bot = %name, "Discord bot connected");
                            }
                            "MESSAGE_CREATE" => match serde_json::from_value::<MessageCreate>(data) {
                                Ok(message) => {
                                    // Answer in a task so slow replies never
                                    // delay heartbeats.
                                    tokio::spawn(on_message(
                                        message,
                                        config.clone(),
                                        credentials.users.clone(),
                                        credentials.channels.clone(),
                                        rest.clone(),
                                        core.clone(),
                                        frigate.clone(),
                                    ));
                                }
                                Err(e) => warn!(error = %e, "unreadable Discord message"),
                            },
                            _ => {}
                        }
                    }
                    Ok(Incoming::HeartbeatRequest) => {
                        sink.send(Message::text(gateway::heartbeat(seq))).await?;
                    }
                    Ok(Incoming::HeartbeatAck) => acked = true,
                    Ok(Incoming::Reconnect) => return Ok(SessionEnd::Reconnect),
                    Ok(Incoming::InvalidSession) => {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        return Ok(SessionEnd::Reconnect);
                    }
                    Ok(Incoming::Hello { .. } | Incoming::Other(_)) => {}
                    Err(e) => warn!(error = %e, "unreadable gateway frame"),
                }
            }
        }
    }
}

async fn next_frame<S>(stream: &mut S) -> anyhow::Result<Option<Incoming>>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
            Err(_) => anyhow::bail!("no Hello from the gateway"),
            Ok(None) => return Ok(None),
            Ok(Some(Err(e))) => anyhow::bail!("gateway: {e}"),
            Ok(Some(Ok(Message::Text(text)))) => {
                return Ok(Some(
                    parse_frame(text.as_str()).map_err(|e| anyhow::anyhow!("{e}"))?,
                ))
            }
            Ok(Some(Ok(_))) => continue,
        }
    }
}

async fn on_message(
    message: MessageCreate,
    config: DiscordConfig,
    users: Vec<u64>,
    channels: Vec<u64>,
    rest: Rest,
    core: CoreHandle,
    frigate: FrigateClient,
) {
    let command = parse_command(&message.content, config.prefix, None);
    if matches!(command, Err(CommandError::NotACommand)) {
        return; // ordinary chat
    }
    let max_age = u64::try_from(config.max_command_age.as_millis()).unwrap_or(u64::MAX);
    let who = &message.author.username;
    match authorize(&message, &users, &channels, unix_ms(), max_age) {
        Decision::IgnoreBot => return,
        Decision::Denied => {
            warn!(user = %message.author.id, %who, channel = %message.channel_id,
                  "ignored a Discord command from a user or channel not in the allow-list");
            return;
        }
        Decision::Stale => {
            warn!(%who, command = %message.content,
                  "ignored a Discord command sent while homeguard was down");
            return;
        }
        Decision::Allowed => {}
    }
    let reply = match command {
        Ok(command) => {
            execute(
                command,
                format!("Discord ({who})"),
                config.prefix,
                &core,
                &frigate,
            )
            .await
        }
        Err(e) => Reply::Text(format!("{e}\n\n{}", help(config.prefix))),
    };
    let channel = Destination::Channel(&message.channel_id);
    match reply {
        Reply::Text(text) => deliver(&rest, channel, &text, None, false).await,
        Reply::Photo { caption, jpeg } => {
            deliver(&rest, channel, &caption, Some(jpeg), false).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn credentials() {
        let config = DiscordConfig::default();
        let none = config.credentials(env(&[])).unwrap();
        assert!(none.webhook.is_none() && none.bot.is_none());

        let full = config
            .credentials(env(&[
                (
                    "DISCORD_WEBHOOK_URL",
                    "https://discord.com/api/webhooks/1/x",
                ),
                ("DISCORD_BOT_TOKEN", "tok"),
                ("DISCORD_ALLOWED_USER_IDS", "111, 222"),
                ("DISCORD_ALLOWED_CHANNEL_IDS", "900"),
            ]))
            .unwrap();
        assert!(full.webhook.is_some());
        let bot = full.bot.unwrap();
        assert_eq!(bot.users, vec![111, 222]);
        assert_eq!(bot.channels, vec![900]);

        let no_users = config.credentials(env(&[("DISCORD_BOT_TOKEN", "tok")]));
        assert!(no_users
            .unwrap_err()
            .to_string()
            .contains("DISCORD_ALLOWED_USER_IDS"));
        let bad_url = config.credentials(env(&[("DISCORD_WEBHOOK_URL", "discord.com/x")]));
        assert!(bad_url.is_err());
    }

    #[test]
    fn payloads_never_ping_and_can_be_silent() {
        let loud = Rest::payload("hi @everyone", false);
        assert_eq!(loud["flags"], 0);
        assert_eq!(loud["allowed_mentions"]["parse"], json!([]));
        assert_eq!(Rest::payload("x", true)["flags"], 4096);
    }
}
