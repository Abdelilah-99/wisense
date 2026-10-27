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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::multipart::{Form, Part};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::chat::{execute, Reply};
use crate::core::time::serde_duration;
use crate::delivery::{scrub, with_retry, ApiError};
use crate::frigate::FrigateClient;
use crate::notify::{
    format_alert, help, is_allowed, parse_command, parse_ids, silent, Alert, CommandError,
};
use crate::run::CoreHandle;

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
        let chats = parse_ids::<i64>(&chats, "Telegram chat id")
            .map_err(|e| anyhow::anyhow!("{}: {e}", self.chat_ids_env))?;
        Ok(Some(Credentials {
            token: token.trim().to_string(),
            chats,
        }))
    }
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
    frigate: FrigateClient,
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
            frigate: FrigateClient::new(frigate_url)?,
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
                Err(ApiError::from_status(status.as_u16(), after, reason))
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

    /// Sends one text message, retrying transient failures; for
    /// `homeguard test-alert`.
    pub async fn send_text(&self, chat: i64, text: &str) -> Result<(), String> {
        with_retry(|| self.send_message(chat, text, false))
            .await
            .map_err(|e| e.to_string())
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

/// Sends every queued alert to every allowed chat.
pub async fn alert_worker(bot: Bot, chats: Vec<i64>, mut alerts: mpsc::Receiver<Alert>) {
    while let Some(alert) = alerts.recv().await {
        let text = format_alert(alert.level, &alert.text);
        let photo = match alert.snapshot_ref.as_deref() {
            Some(id) => match bot.frigate.event_snapshot(id).await {
                Ok(photo) => Some(photo),
                Err(e) => {
                    warn!(error = %e, "Frigate snapshot unavailable; sending text only");
                    None
                }
            },
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
            let reply = match parse_command(text, '/', me.as_deref()) {
                Ok(command) => {
                    execute(
                        command,
                        format!("Telegram ({who})"),
                        '/',
                        &core,
                        &bot.frigate,
                    )
                    .await
                }
                Err(CommandError::NotACommand | CommandError::OtherBot) => continue,
                Err(e) => Reply::Text(format!("{e}\n\n{}", help('/'))),
            };
            match reply {
                Reply::Text(text) => bot.deliver(chat, &text, None, false).await,
                Reply::Photo { caption, jpeg } => {
                    bot.deliver(chat, &caption, Some(jpeg), false).await
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
