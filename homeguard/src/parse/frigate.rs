//! Frigate messages.
//!
//! - `frigate/events`: `{"type": "new"|"update"|"end", "before": {..},
//!   "after": {"id", "camera", "label", "score", "top_score",
//!   "false_positive", ..}}`.
//! - `frigate/available`: `online`, `offline` (MQTT will) or `stopped`.
//! - `frigate/<camera>/status/detect`: `online`, `offline` or `disabled`.

use serde::Deserialize;

use super::{utf8, ParseError};
use crate::core::Event;

#[derive(Debug, Deserialize)]
struct EventMessage {
    #[serde(rename = "type")]
    kind: String,
    after: Option<TrackedObject>,
}

#[derive(Debug, Deserialize)]
struct TrackedObject {
    id: String,
    camera: String,
    label: String,
    #[serde(default)]
    score: Option<f32>,
    #[serde(default)]
    top_score: Option<f32>,
    #[serde(default)]
    false_positive: bool,
}

/// `rest` is the topic after `frigate/`. `cameras` are all configured
/// cameras, which Frigate-wide availability applies to.
pub fn parse(rest: &str, payload: &[u8], cameras: &[String]) -> Result<Vec<Event>, ParseError> {
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        ["events"] => event(payload),
        ["available"] => {
            let online = status(payload)?;
            Ok(cameras
                .iter()
                .map(|camera| availability(camera.clone(), online))
                .collect())
        }
        [camera, "status", "detect"] if !camera.is_empty() => {
            Ok(vec![availability(camera.to_string(), status(payload)?)])
        }
        _ => Ok(Vec::new()),
    }
}

fn event(payload: &[u8]) -> Result<Vec<Event>, ParseError> {
    let message: EventMessage =
        serde_json::from_slice(payload).map_err(|e| ParseError::Json(e.to_string()))?;
    if !matches!(message.kind.as_str(), "new" | "update") {
        return Ok(Vec::new());
    }
    let Some(object) = message.after else {
        return Err(ParseError::Field {
            field: "after",
            problem: "is missing",
        });
    };
    if object.label != "person" || object.false_positive {
        return Ok(Vec::new());
    }
    let score = match (object.score, object.top_score) {
        (Some(a), Some(b)) => a.max(b),
        (Some(s), None) | (None, Some(s)) => s,
        (None, None) => {
            return Err(ParseError::Field {
                field: "score",
                problem: "is missing",
            })
        }
    };
    if !(0.0..=1.0).contains(&score) {
        return Err(ParseError::Field {
            field: "score",
            problem: "is not between 0 and 1",
        });
    }
    Ok(vec![Event::PersonDetected {
        camera: object.camera,
        score,
        snapshot_ref: Some(object.id),
    }])
}

/// `true` for online. `disabled` counts as offline: a camera switched off
/// while armed deserves the same scrutiny as one unplugged.
fn status(payload: &[u8]) -> Result<bool, ParseError> {
    match utf8(payload)?.trim() {
        "online" => Ok(true),
        "offline" | "stopped" | "disabled" => Ok(false),
        other => Err(ParseError::UnexpectedValue(other.to_string())),
    }
}

fn availability(device: String, online: bool) -> Event {
    if online {
        Event::DeviceOnline { device }
    } else {
        Event::DeviceOffline { device }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cameras() -> Vec<String> {
        vec!["front_cam".into(), "garden_cam".into()]
    }

    fn event_json(kind: &str, label: &str, score: f32, top: f32, fp: bool) -> String {
        format!(
            r#"{{"type":"{kind}","before":{{}},"after":{{"id":"1700000000.1-abc","camera":"front_cam","label":"{label}","score":{score},"top_score":{top},"false_positive":{fp},"has_snapshot":true}}}}"#
        )
    }

    #[test]
    fn person_events_use_the_best_score() {
        let events = parse(
            "events",
            event_json("new", "person", 0.61, 0.84, false).as_bytes(),
            &cameras(),
        )
        .unwrap();
        assert_eq!(
            events,
            vec![Event::PersonDetected {
                camera: "front_cam".into(),
                score: 0.84,
                snapshot_ref: Some("1700000000.1-abc".into())
            }]
        );
        let update = event_json("update", "person", 0.9, 0.8, false);
        assert_eq!(
            parse("events", update.as_bytes(), &cameras())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn ignores_end_other_labels_and_false_positives() {
        for json in [
            event_json("end", "person", 0.9, 0.9, false),
            event_json("new", "car", 0.9, 0.9, false),
            event_json("new", "person", 0.9, 0.9, true),
        ] {
            assert_eq!(parse("events", json.as_bytes(), &cameras()), Ok(vec![]));
        }
    }

    #[test]
    fn frigate_availability_applies_to_every_camera() {
        assert_eq!(
            parse("available", b"offline", &cameras()),
            Ok(vec![
                Event::DeviceOffline {
                    device: "front_cam".into()
                },
                Event::DeviceOffline {
                    device: "garden_cam".into()
                },
            ])
        );
        assert_eq!(parse("available", b"online", &cameras()).unwrap().len(), 2);
        assert_eq!(parse("available", b"stopped", &cameras()).unwrap().len(), 2);
    }

    #[test]
    fn camera_status() {
        assert_eq!(
            parse("front_cam/status/detect", b"offline", &cameras()),
            Ok(vec![Event::DeviceOffline {
                device: "front_cam".into()
            }])
        );
        assert_eq!(
            parse("front_cam/status/detect", b"disabled", &cameras()),
            Ok(vec![Event::DeviceOffline {
                device: "front_cam".into()
            }])
        );
        assert_eq!(
            parse("front_cam/status/detect", b"online", &cameras()),
            Ok(vec![Event::DeviceOnline {
                device: "front_cam".into()
            }])
        );
        // Other Frigate topics are not ours.
        assert_eq!(parse("front_cam/person", b"1", &cameras()), Ok(vec![]));
        assert_eq!(parse("stats", b"{}", &cameras()), Ok(vec![]));
    }

    #[test]
    fn malformed_messages_are_errors() {
        assert!(parse("events", b"garbage", &cameras()).is_err());
        assert!(parse("events", br#"{"type":"new"}"#, &cameras()).is_err());
        assert!(parse(
            "events",
            br#"{"type":"new","after":{"id":"x","camera":"c","label":"person"}}"#,
            &cameras()
        )
        .is_err());
        assert!(parse(
            "events",
            br#"{"type":"new","after":{"id":"x","camera":"c","label":"person","score":7}}"#,
            &cameras()
        )
        .is_err());
        assert!(parse("available", b"sleepy", &cameras()).is_err());
        assert!(parse("front_cam/status/detect", &[0xc3, 0x28], &cameras()).is_err());
    }
}
