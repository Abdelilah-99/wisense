//! The Discord bot and webhook against an in-process fake of Discord: a
//! WebSocket gateway and a REST server, both on loopback. No network needed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use homeguard::core::{Command, Mode};
use homeguard::discord::gateway::snowflake_at;
use homeguard::discord::{self, BotCredentials, DiscordConfig, Rest};
use homeguard::frigate::FrigateClient;
use homeguard::run::{unix_ms, CoreHandle, Snapshot};
use homeguard::status::Status;

#[derive(Debug, Clone)]
struct Posted {
    path: String,
    query: Option<String>,
    authorization: Option<String>,
    content_type: String,
    body: Bytes,
}

#[derive(Clone, Default)]
struct Fake {
    posts: Arc<Mutex<Vec<Posted>>>,
    rate_limited_once: Arc<Mutex<bool>>,
}

async fn record(
    State(fake): State<Fake>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    // The first bot message is rate limited, to exercise retry_after.
    if uri.path().contains("/channels/") {
        let mut limited = fake.rate_limited_once.lock().unwrap();
        if !*limited {
            *limited = true;
            return (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({"message": "You are being rate limited.", "retry_after": 0.05, "global": false})),
            )
                .into_response();
        }
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    fake.posts.lock().unwrap().push(Posted {
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        authorization: header("authorization"),
        content_type: header("content-type").unwrap_or_default(),
        body,
    });
    axum::Json(json!({"id": "1"})).into_response()
}

async fn rest_server() -> (String, Fake) {
    let fake = Fake::default();
    let app = Router::new()
        .route("/api/v10/channels/{channel}/messages", post(record))
        .route("/api/webhooks/{id}/{token}", post(record))
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

fn dispatch(seq: u64, kind: &str, data: Value) -> Message {
    Message::text(json!({"op": 0, "s": seq, "t": kind, "d": data}).to_string())
}

fn message(created_ms: u64, author: &str, name: &str, bot: bool, content: &str) -> Value {
    json!({
        "id": snowflake_at(created_ms).to_string(),
        "channel_id": "900",
        "guild_id": "55",
        "author": {"id": author, "username": name, "bot": bot},
        "content": content,
    })
}

#[derive(Default)]
struct GatewayLog {
    identifies: Vec<Value>,
    heartbeats: usize,
}

/// A fake gateway: Hello, then READY and the scripted messages on the first
/// connection, heartbeat acks, and a Reconnect request once the messages
/// are out. Later connections only get Hello and acks.
async fn gateway_server(script: Vec<Value>) -> (String, Arc<Mutex<GatewayLog>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(GatewayLog::default()));
    let shared = log.clone();
    tokio::spawn(async move {
        let mut connection = 0;
        while let Ok((tcp, _)) = listener.accept().await {
            connection += 1;
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let log = shared.clone();
            let script = script.clone();
            tokio::spawn(async move {
                ws.send(Message::text(r#"{"op":10,"d":{"heartbeat_interval":150}}"#))
                    .await
                    .unwrap();
                let mut sent_script = false;
                while let Some(Ok(frame)) = ws.next().await {
                    let Message::Text(text) = frame else { continue };
                    let value: Value = serde_json::from_str(text.as_str()).unwrap();
                    match value["op"].as_u64() {
                        Some(2) => {
                            log.lock().unwrap().identifies.push(value);
                            if connection == 1 && !sent_script {
                                sent_script = true;
                                ws.send(dispatch(
                                    1,
                                    "READY",
                                    json!({"user": {"username": "homeguard"}}),
                                ))
                                .await
                                .unwrap();
                                for (i, m) in script.iter().enumerate() {
                                    ws.send(dispatch(2 + i as u64, "MESSAGE_CREATE", m.clone()))
                                        .await
                                        .unwrap();
                                }
                                // Ask for a heartbeat right away.
                                ws.send(Message::text(r#"{"op":1,"d":null}"#))
                                    .await
                                    .unwrap();
                            }
                        }
                        Some(1) => {
                            let count = {
                                let mut log = log.lock().unwrap();
                                log.heartbeats += 1;
                                log.heartbeats
                            };
                            ws.send(Message::text(r#"{"op":11}"#)).await.unwrap();
                            if connection == 1 && count == 3 {
                                ws.send(Message::text(r#"{"op":7,"d":null}"#))
                                    .await
                                    .unwrap();
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (format!("ws://{addr}/"), log)
}

fn snapshot() -> Snapshot {
    Snapshot::new(
        Status {
            state: "disarmed".into(),
            mode: None,
            cause: None,
            snapshot_ref: None,
            zones: vec![],
            presence: vec![],
        },
        vec![],
    )
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn bot_answers_only_allowed_fresh_commands_and_reconnects() {
    let now = unix_ms();
    let script = vec![
        message(now, "111", "alice", false, "!arm night"),
        message(now, "222", "mallory", false, "!disarm"),
        message(now, "333", "otherbot", true, "!disarm"),
        message(now - 3_600_000, "111", "alice", false, "!disarm"),
        message(now, "111", "alice", false, "just chatting"),
        message(now, "111", "alice", false, "!status"),
        message(now, "111", "alice", false, "!fly"),
    ];
    let (rest_url, fake) = rest_server().await;
    let (gateway_url, gateway) = gateway_server(script).await;
    let config = DiscordConfig {
        api_url: format!("{rest_url}/api/v10"),
        gateway_url,
        ..DiscordConfig::default()
    };
    let rest = Rest::new(&config, Some("test-token")).unwrap();
    let (core, mut commands) = CoreHandle::for_tests(snapshot());
    let credentials = BotCredentials {
        token: "test-token".into(),
        users: vec![111],
        channels: vec![900],
    };
    tokio::spawn(discord::bot_loop(
        config,
        credentials,
        rest,
        core,
        FrigateClient::new(&rest_url).unwrap(),
    ));

    // Only alice's fresh !arm reaches the core.
    let (command, by) = tokio::time::timeout(Duration::from_secs(5), commands.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(command, Command::Arm(Mode::Night));
    assert_eq!(by, "Discord (alice)");

    // Replies: !arm, !status and the unknown !fly (with help).
    wait_for(|| fake.posts.lock().unwrap().len() >= 3).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(commands.try_recv().is_err(), "no other command may run");
    let posts = fake.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 3, "{posts:?}");
    let texts: Vec<String> = posts
        .iter()
        .map(|p| {
            assert_eq!(p.path, "/api/v10/channels/900/messages");
            assert_eq!(p.authorization.as_deref(), Some("Bot test-token"));
            let body: Value = serde_json::from_slice(&p.body).unwrap();
            assert_eq!(body["allowed_mentions"]["parse"], json!([]));
            body["content"].as_str().unwrap().to_string()
        })
        .collect();
    assert!(texts
        .iter()
        .all(|t| t.starts_with("State: disarmed") || t.contains("!status - ")));
    assert!(texts
        .iter()
        .any(|t| t.contains("unknown command") && t.contains("!arm away")));

    // Identified with the token and intents, heartbeats answered (including
    // the one Discord asked for), then a reconnect when told to.
    wait_for(|| gateway.lock().unwrap().identifies.len() >= 2).await;
    let log = gateway.lock().unwrap();
    assert_eq!(log.identifies[0]["d"]["token"], "test-token");
    assert_eq!(log.identifies[0]["d"]["intents"], 37_376);
    assert!(log.heartbeats >= 3);
}

#[tokio::test]
async fn webhook_sends_text_photos_and_silent_messages() {
    let (rest_url, fake) = rest_server().await;
    let config = DiscordConfig::default();
    let rest = Rest::new(&config, None).unwrap();
    let url = format!("{rest_url}/api/webhooks/123/secret-part");

    rest.webhook(&url, "🚨 ALARM", None, false).await.unwrap();
    rest.webhook(&url, "· notice", None, true).await.unwrap();
    rest.webhook(&url, "🚨 with photo", Some(b"\xff\xd8JPEG".to_vec()), false)
        .await
        .unwrap();

    let posts = fake.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 3);
    for p in &posts {
        assert_eq!(p.path, "/api/webhooks/123/secret-part");
        assert_eq!(p.query.as_deref(), Some("wait=true"));
        assert!(p.authorization.is_none());
    }
    let first: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(first["content"], "🚨 ALARM");
    assert_eq!(first["flags"], 0);
    let second: Value = serde_json::from_slice(&posts[1].body).unwrap();
    assert_eq!(second["flags"], 4096);
    assert!(posts[2].content_type.starts_with("multipart/form-data"));
    let body = String::from_utf8_lossy(&posts[2].body);
    assert!(body.contains("name=\"payload_json\""));
    assert!(body.contains("name=\"files[0]\"; filename=\"snapshot.jpg\""));
    assert!(body.contains("with photo"));
}

#[tokio::test]
async fn webhook_rejections_are_reported_without_the_secret() {
    let config = DiscordConfig::default();
    let rest = Rest::new(&config, None).unwrap();
    // Nothing listens on port 9 of loopback.
    let err = rest
        .webhook(
            "http://127.0.0.1:9/api/webhooks/1/super-secret",
            "x",
            None,
            false,
        )
        .await
        .unwrap_err();
    assert!(!err.to_string().contains("super-secret"), "{err}");
}
