//! Telegram: alerts out, commands in, through the Bot API.
//!
//! Alerts go through a queue to a worker, so the core never waits on the
//! network. Each alert is sent to every allowed chat, as a photo with the
//! Frigate snapshot when there is one (text if the snapshot cannot be
//! fetched or sent), retried with backoff.
//!
//! Commands are long-polled with `getUpdates`. Only chats in the allow-list
//! are answered; everyone else is ignored and logged. The bot token is part
//! of every API URL, so errors are always stripped of their URL before they
//! are logged.

pub mod command;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::multipart::{Form, Part};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::core::time::serde_duration;
use crate::core::{AlertLevel, Command};
use crate::run::CoreHandle;
use command::{
    format_alert, format_events, format_status, is_allowed, parse_chat_ids, parse_command,
    safe_path_segment, silent, BotCommand, CommandError, HELP,
};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TelegramConfig {
    /// Environment variable holding the bot token from BotFather.
    pub token_env: String,
    /// Environment variable holding the allowed chat ids, comma-separated.
    /// Alerts go to all of them; only they may send commands.
    pub chat_ids_env: String,
    pub api_url: String,
    #[serde(with = "serde_duration")]
    pub poll_timeout: Duration,
    /// Commands older than this (sent while the hub was down) are ignored.
    #[serde(with = "serde_duration")]
    pub max_command_age: Duration,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        TelegramConfig {
            token_env: "TELEGRAM_BOT_TOKEN".into(),
            chat_ids_env: "TELEGRAM_CHAT_IDS".into(),
            api_url: "https://api.telegram.org".into(),
            poll_timeout: Duration::from_secs(30),
            max_command_age: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub token: String,
    pub chats: Vec<i64>,
}

impl TelegramConfig {
    /// `None` when no token is set (Telegram disabled). A token without a
    /// valid chat list is an error: alerts would go nowhere.
    pub fn credentials(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Option<Credentials>> {
        let Some(token) = env(&self.token_env).filter(|t| !t.trim().is_empty()) else {
            return Ok(None);
        };
        let chats = env(&self.chat_ids_env).unwrap_or_default();
        let chats =
            parse_chat_ids(&chats).map_err(|e| anyhow::anyhow!("{}: {e}", self.chat_ids_env))?;
        Ok(Some(Credentials {
            token: token.trim().to_string(),
            chats,
        }))
    }
}

/// An alert for the worker.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub level: AlertLevel,
    pub text: String,
    pub snapshot_ref: Option<String>,
}

#[derive(Debug)]
enum ApiError {
    /// Worth retrying (network, rate limit, server error).
    Retry {
        after: Option<Duration>,
        reason: String,
    },
    /// Retrying will not help (bad request, blocked by the user).
    Fatal(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Retry { reason, .. } | ApiError::Fatal(reason) => f.write_str(reason),
        }
    }
}

/// Never let the token reach a log line.
fn scrub(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    parameters: Option<ResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    chat: Chat,
    #[serde(default)]
    from: Option<User>,
    #[serde(default)]
    text: Option<String>,
    /// Unix seconds.
    date: u64,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct User {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Me {
    username: Option<String>,
}

#[derive(Clone)]
pub struct Bot {
    http: reqwest::Client,
    /// `<api_url>/bot<token>`: secret, never logged.
    base: String,
    frigate_url: String,
}

impl Bot {
    pub fn new(config: &TelegramConfig, token: &str, frigate_url: &str) -> anyhow::Result<Bot> {
        let http = reqwest::Client::builder()
            .timeout(config.poll_timeout + Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow::anyhow!("HTTP client: {}", scrub(e)))?;
        Ok(Bot {
            http,
            base: format!("{}/bot{token}", config.api_url.trim_end_matches('/')),
            frigate_url: frigate_url.trim_end_matches('/').to_string(),
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<T, ApiError> {
        let response = request.send().await.map_err(|e| ApiError::Retry {
            after: None,
            reason: format!("{method}: {}", scrub(e)),
        })?;
        let status = response.status();
        let body: ApiResponse<T> = response.json().await.map_err(|e| ApiError::Retry {
            after: None,
            reason: format!("{method}: unreadable response ({status}): {}", scrub(e)),
        })?;
        match (body.ok, body.result) {
            (true, Some(result)) => Ok(result),
            _ => {
                let reason = format!(
                    "{method}: {status} {}",
                    body.description.unwrap_or_default()
                );
                let after = body
                    .parameters
                    .and_then(|p| p.retry_after)
                    .map(Duration::from_secs);
                if status.as_u16() == 429 || status.is_server_error() {
                    Err(ApiError::Retry { after, reason })
                } else {
                    Err(ApiError::Fatal(reason))
                }
            }
        }
    }

    async fn send_message(&self, chat: i64, text: &str, quiet: bool) -> Result<(), ApiError> {
        let body = json!({ "chat_id": chat, "text": text, "disable_notification": quiet });
        let request = self
            .http
            .post(format!("{}/sendMessage", self.base))
            .json(&body);
        self.call::<serde_json::Value>("sendMessage", request)
            .await
            .map(|_| ())
    }

    async fn send_photo(
        &self,
        chat: i64,
        photo: Vec<u8>,
        caption: &str,
        quiet: bool,
    ) -> Result<(), ApiError> {
        let part = Part::bytes(photo)
            .file_name("snapshot.jpg")
            .mime_str("image/jpeg")
            .map_err(|e| ApiError::Fatal(scrub(e)))?;
        // Captions are limited to 1024 characters.
        let caption: String = caption.chars().take(1024).collect();
        let form = Form::new()
            .text("chat_id", chat.to_string())
            .text("caption", caption)
            .text("disable_notification", quiet.to_string())
            .part("photo", part);
        let request = self
            .http
            .post(format!("{}/sendPhoto", self.base))
            .multipart(form);
        self.call::<serde_json::Value>("sendPhoto", request)
            .await
            .map(|_| ())
    }

    /// A JPEG from Frigate's API (`path` after `/api/`).
    async fn frigate_jpeg(&self, path: &str) -> Result<Vec<u8>, String> {
        let url = format!("{}/api/{path}", self.frigate_url);
        let response = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{url}: {}", response.status()));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return Err(format!("{url}: empty"));
        }
        Ok(bytes.to_vec())
    }

    /// Sends text, or a photo with the text as caption, retrying transient
    /// failures. A photo that cannot be sent falls back to text.
    async fn deliver(&self, chat: i64, text: &str, photo: Option<Vec<u8>>, quiet: bool) {
        if let Some(photo) = photo {
            match with_retry(|| self.send_photo(chat, photo.clone(), text, quiet)).await {
                Ok(()) => return,
                Err(e) => warn!(chat, error = %e, "sending the snapshot failed; sending text"),
            }
        }
        if let Err(e) = with_retry(|| self.send_message(chat, text, quiet)).await {
            warn!(chat, error = %e, "Telegram message not delivered");
        }
    }
}

const MAX_ATTEMPTS: u32 = 5;

async fn with_retry<F, Fut>(mut attempt: F) -> Result<(), ApiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), ApiError>>,
{
    let mut backoff = crate::mqtt::Backoff::new(Duration::from_secs(30));
    let mut tries = 0;
    loop {
        tries += 1;
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(ApiError::Retry { after, reason }) if tries < MAX_ATTEMPTS => {
                let delay = after.unwrap_or_else(|| backoff.next_delay());
                debug!(%reason, retry_in = ?delay, "Telegram call failed; retrying");
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Sends every queued alert to every allowed chat.
pub async fn alert_worker(bot: Bot, chats: Vec<i64>, mut alerts: mpsc::Receiver<Alert>) {
    while let Some(alert) = alerts.recv().await {
        let text = format_alert(alert.level, &alert.text);
        let photo = match alert.snapshot_ref.as_deref() {
            Some(id) if safe_path_segment(id) => {
                match bot.frigate_jpeg(&format!("events/{id}/snapshot.jpg")).await {
                    Ok(photo) => Some(photo),
                    Err(e) => {
                        warn!(error = %e, "Frigate snapshot unavailable; sending text only");
                        None
                    }
                }
            }
            Some(id) => {
                warn!(snapshot_ref = %id, "ignoring a snapshot reference with unexpected characters");
                None
            }
            None => None,
        };
        for chat in &chats {
            bot.deliver(*chat, &text, photo.clone(), silent(alert.level))
                .await;
        }
    }
}

/// Whether a message sent at `date` (Unix seconds) is too old to act on.
pub fn is_stale(date: u64, now: SystemTime, max_age: Duration) -> bool {
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    now.saturating_sub(date) > max_age.as_secs()
}

/// Long-polls for commands and answers allowed chats.
pub async fn command_loop(bot: Bot, config: TelegramConfig, chats: Vec<i64>, core: CoreHandle) {
    let me = match bot
        .call::<Me>("getMe", bot.http.post(format!("{}/getMe", bot.base)))
        .await
    {
        Ok(me) => me.username,
        Err(e) => {
            warn!(error = %e, "Telegram getMe failed (check the bot token)");
            None
        }
    };
    info!(bot = ?me, chats = chats.len(), "Telegram commands enabled");
    let mut offset: i64 = 0;
    let mut backoff = crate::mqtt::Backoff::new(Duration::from_secs(60));
    loop {
        let body = json!({
            "offset": offset,
            "timeout": config.poll_timeout.as_secs(),
            "allowed_updates": ["message"],
        });
        let request = bot
            .http
            .post(format!("{}/getUpdates", bot.base))
            .json(&body);
        let updates: Vec<Update> = match bot.call("getUpdates", request).await {
            Ok(updates) => {
                backoff.reset();
                updates
            }
            Err(e) => {
                let delay = match &e {
                    ApiError::Retry {
                        after: Some(after), ..
                    } => *after,
                    _ => backoff.next_delay(),
                };
                warn!(error = %e, retry_in = ?delay, "Telegram getUpdates failed");
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        for update in updates {
            offset = offset.max(update.update_id + 1);
            let Some(message) = update.message else {
                continue;
            };
            let Some(text) = message.text.as_deref() else {
                continue;
            };
            let chat = message.chat.id;
            let who = message
                .from
                .as_ref()
                .and_then(|u| u.username.clone().or_else(|| u.first_name.clone()))
                .unwrap_or_else(|| "unknown".into());
            if !is_allowed(chat, &chats) {
                warn!(chat, from = %who, "ignored a Telegram message from a chat not in the allow-list");
                continue;
            }
            if is_stale(message.date, SystemTime::now(), config.max_command_age) {
                warn!(chat, from = %who, command = %text, "ignored a Telegram command sent while homeguard was down");
                continue;
            }
            let reply = match parse_command(text, me.as_deref()) {
                Ok(command) => answer(&bot, &core, chat, command, &who).await,
                Err(CommandError::NotACommand | CommandError::OtherBot) => continue,
                Err(e) => Some(format!("{e}\n\n{HELP}")),
            };
            if let Some(reply) = reply {
                bot.deliver(chat, &reply, None, false).await;
            }
        }
    }
}

/// Runs one command; the reply text, if any (a snapshot replies itself).
async fn answer(
    bot: &Bot,
    core: &CoreHandle,
    chat: i64,
    command: BotCommand,
    who: &str,
) -> Option<String> {
    info!(chat, from = %who, ?command, "Telegram command");
    let by = format!("Telegram ({who})");
    match command {
        BotCommand::Status => Some(format_status(&core.snapshot().status)),
        BotCommand::Events => Some(format_events(
            &core.snapshot().recent(10),
            crate::run::unix_ms(),
        )),
        BotCommand::Help => Some(HELP.to_string()),
        BotCommand::Arm(mode) => Some(match core.command(Command::Arm(mode), by).await {
            Ok(snapshot) => format_status(&snapshot.status),
            Err(e) => format!("Failed: {e}"),
        }),
        BotCommand::Disarm => Some(match core.command(Command::Disarm, by).await {
            Ok(snapshot) => format_status(&snapshot.status),
            Err(e) => format!("Failed: {e}"),
        }),
        BotCommand::Snapshot(camera) => {
            let known = core.snapshot().status.camera_ids().any(|c| c == camera);
            if !known || !safe_path_segment(&camera) {
                let cameras: Vec<String> = core
                    .snapshot()
                    .status
                    .camera_ids()
                    .map(str::to_string)
                    .collect();
                return Some(format!(
                    "Unknown camera {camera:?}. Cameras: {}",
                    cameras.join(", ")
                ));
            }
            match bot.frigate_jpeg(&format!("{camera}/latest.jpg")).await {
                Ok(photo) => {
                    bot.deliver(chat, &camera, Some(photo), false).await;
                    None
                }
                Err(e) => {
                    warn!(error = %e, "snapshot failed");
                    Some(format!(
                        "Could not get a picture from {camera} (is Frigate running?)"
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials() {
        let config = TelegramConfig::default();
        assert!(config.credentials(|_| None).unwrap().is_none());
        let env = |name: &str| match name {
            "TELEGRAM_BOT_TOKEN" => Some("token".to_string()),
            "TELEGRAM_CHAT_IDS" => Some("1, 2".to_string()),
            _ => None,
        };
        let creds = config.credentials(env).unwrap().unwrap();
        assert_eq!(creds.chats, vec![1, 2]);
        let no_chats = |name: &str| (name == "TELEGRAM_BOT_TOKEN").then(|| "token".to_string());
        let err = config.credentials(no_chats).unwrap_err();
        assert!(err.to_string().contains("TELEGRAM_CHAT_IDS"));
    }

    #[test]
    fn stale_commands() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let max = Duration::from_secs(120);
        assert!(!is_stale(10_000, now, max));
        assert!(!is_stale(9_880, now, max));
        assert!(is_stale(9_879, now, max));
        // A clock slightly behind Telegram's: not stale.
        assert!(!is_stale(10_005, now, max));
    }
}
