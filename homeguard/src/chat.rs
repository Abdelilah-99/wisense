//! Running a bot command against the core: shared by the Telegram and
//! Discord bots, so they behave the same.

use tracing::{info, warn};

use crate::core::Command;
use crate::frigate::FrigateClient;
use crate::notify::{format_events, format_status, help, BotCommand};
use crate::run::{unix_ms, CoreHandle};

/// What to answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Text(String),
    Photo { caption: String, jpeg: Vec<u8> },
}

/// Runs `command` on behalf of `by` (e.g. "Discord (alice)") and returns
/// the answer. `prefix` is the bot's command prefix, for the help text.
pub async fn execute(
    command: BotCommand,
    by: String,
    prefix: char,
    core: &CoreHandle,
    frigate: &FrigateClient,
) -> Reply {
    info!(%by, ?command, "bot command");
    match command {
        BotCommand::Status => Reply::Text(format_status(&core.snapshot().status)),
        BotCommand::Events => Reply::Text(format_events(&core.snapshot().recent(10), unix_ms())),
        BotCommand::Help => Reply::Text(help(prefix)),
        BotCommand::Arm(mode) => Reply::Text(match core.command(Command::Arm(mode), by).await {
            Ok(snapshot) => format_status(&snapshot.status),
            Err(e) => format!("Failed: {e}"),
        }),
        BotCommand::Disarm => Reply::Text(match core.command(Command::Disarm, by).await {
            Ok(snapshot) => format_status(&snapshot.status),
            Err(e) => format!("Failed: {e}"),
        }),
        BotCommand::Snapshot(camera) => {
            let cameras: Vec<String> = core
                .snapshot()
                .status
                .camera_ids()
                .map(str::to_string)
                .collect();
            if !cameras.contains(&camera) {
                return Reply::Text(format!(
                    "Unknown camera {camera:?}. Cameras: {}",
                    cameras.join(", ")
                ));
            }
            match frigate.latest(&camera).await {
                Ok(jpeg) => Reply::Photo {
                    caption: camera,
                    jpeg,
                },
                Err(e) => {
                    warn!(error = %e, "snapshot failed");
                    Reply::Text(format!(
                        "Could not get a picture from {camera} (is Frigate running?)"
                    ))
                }
            }
        }
    }
}
