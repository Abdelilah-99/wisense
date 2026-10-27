//! The MQTT connection: incoming messages become core events, outgoing
//! messages are queued without ever blocking the core.

use std::time::Duration;

use rumqttc::{AsyncClient, ConnectionError, EventLoop, LastWill, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::MqttConfig;
use crate::core::Event;
use crate::parse::Mapping;
use crate::publish::{Outgoing, AVAILABILITY_TOPIC};

/// A last-will message.
pub struct Will {
    pub topic: String,
    pub payload: Vec<u8>,
    /// Retained wills suit a state topic such as homeguard/availability; an
    /// event stream must not keep a stale "offline" for new subscribers.
    pub retain: bool,
}

/// Connection settings for one client.
pub struct Connection {
    pub client_id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub keep_alive: Duration,
    /// Published by the broker if this client disappears.
    pub last_will: Option<Will>,
}

impl Connection {
    pub fn from_config(config: &MqttConfig, username: String, password: String) -> Self {
        Connection {
            client_id: config.client_id.clone(),
            host: config.host.clone(),
            port: config.port,
            username,
            password,
            keep_alive: config.keep_alive,
            last_will: Some(Will {
                topic: AVAILABILITY_TOPIC.into(),
                payload: b"offline".to_vec(),
                retain: true,
            }),
        }
    }

    pub fn open(self) -> (AsyncClient, EventLoop) {
        let mut options = MqttOptions::new(self.client_id, self.host, self.port);
        options.set_credentials(self.username, self.password);
        options.set_keep_alive(self.keep_alive);
        if let Some(will) = self.last_will {
            options.set_last_will(LastWill::new(
                will.topic,
                will.payload,
                QoS::AtLeastOnce,
                will.retain,
            ));
        }
        AsyncClient::new(options, 256)
    }
}

/// Queues a message; drops it with a warning if the client queue is full,
/// rather than making the caller wait.
pub fn publish(client: &AsyncClient, message: Outgoing) {
    let topic = message.topic.clone();
    if let Err(e) = client.try_publish(
        message.topic,
        QoS::AtLeastOnce,
        message.retain,
        message.payload,
    ) {
        warn!(%topic, error = %e, "MQTT publish dropped");
    }
}

/// Exponential backoff between reconnection attempts, capped.
#[derive(Debug, Clone, PartialEq)]
pub struct Backoff {
    next: Duration,
    max: Duration,
}

impl Backoff {
    pub fn new(max: Duration) -> Self {
        Backoff {
            next: Duration::from_secs(1),
            max,
        }
    }

    pub fn reset(&mut self) {
        self.next = Duration::from_secs(1);
    }

    /// The delay to wait now; the next one doubles.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }
}

/// Drives the connection forever: (re)subscribes after every connect, turns
/// messages into events, and reconnects with backoff after errors (rumqttc
/// reconnects on the next poll).
pub async fn run_incoming(
    mut eventloop: EventLoop,
    client: AsyncClient,
    mapping: Mapping,
    events: mpsc::Sender<Event>,
) {
    let mut backoff = Backoff::new(Duration::from_secs(60));
    loop {
        match eventloop.poll().await {
            Ok(rumqttc::Event::Incoming(Packet::ConnAck(_))) => {
                info!("connected to MQTT broker");
                backoff.reset();
                for topic in mapping.subscriptions() {
                    if let Err(e) = client.try_subscribe(topic.clone(), QoS::AtLeastOnce) {
                        warn!(%topic, error = %e, "subscribe failed");
                    }
                }
                publish(
                    &client,
                    Outgoing {
                        topic: AVAILABILITY_TOPIC.into(),
                        payload: b"online".to_vec(),
                        retain: true,
                    },
                );
            }
            Ok(rumqttc::Event::Incoming(Packet::Publish(message))) => {
                match mapping.parse(&message.topic, &message.payload) {
                    Ok(parsed) => {
                        for event in parsed {
                            debug!(topic = %message.topic, ?event, "event");
                            if events.send(event).await.is_err() {
                                return; // the core stopped
                            }
                        }
                    }
                    Err(e) => warn!(
                        topic = %message.topic,
                        error = %e,
                        "dropping malformed message"
                    ),
                }
            }
            Ok(_) => {}
            Err(e) => {
                let delay = backoff.next_delay();
                warn!(error = %describe(&e), retry_in = ?delay, "MQTT connection lost");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn describe(error: &ConnectionError) -> String {
    match error {
        ConnectionError::ConnectionRefused(code) => {
            format!("broker refused the connection ({code:?}): check the MQTT user and password")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(10));
        let delays: Vec<u64> = (0..6).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 10, 10]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }
}
