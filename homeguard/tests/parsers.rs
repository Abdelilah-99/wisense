//! Parse real-looking MQTT messages from tests/fixtures/mqtt through the
//! same router `homeguard run` uses.

use std::collections::BTreeMap;

use homeguard::core::{Event, MotionSource};
use homeguard::parse::{Mapping, ParseError};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/mqtt/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn mapping() -> Mapping {
    Mapping {
        zigbee_devices: BTreeMap::from([("0x00158d0008a1b2c3".into(), "front_door".into())]),
        cameras: vec!["front_cam".into(), "garden_cam".into()],
        wisense_links: BTreeMap::from([("wlp2s0".into(), "living_wisense".into())]),
        ..Mapping::default()
    }
}

fn parse(topic: &str, name: &str) -> Result<Vec<Event>, ParseError> {
    mapping().parse(topic, &fixture(name))
}

#[test]
fn zigbee_devices() {
    let open = Ok(vec![Event::ContactChanged {
        sensor: "front_door".into(),
        open: true,
    }]);
    assert_eq!(
        parse("zigbee2mqtt/0x00158d0008a1b2c3", "zigbee_contact_open.json"),
        open
    );
    assert_eq!(
        parse("zigbee2mqtt/front_door", "zigbee_contact_open.json"),
        open
    );
    assert_eq!(
        parse("zigbee2mqtt/front_door", "zigbee_contact_closed.json"),
        Ok(vec![Event::ContactChanged {
            sensor: "front_door".into(),
            open: false
        }])
    );
    assert_eq!(
        parse("zigbee2mqtt/hall_pir", "zigbee_pir_occupancy.json"),
        Ok(vec![Event::MotionDetected {
            source: MotionSource::Pir,
            sensor: "hall_pir".into()
        }])
    );
    assert_eq!(
        parse("zigbee2mqtt/hall_pir", "zigbee_pir_clear.json"),
        Ok(vec![])
    );
    assert_eq!(
        parse("zigbee2mqtt/kitchen_smoke", "zigbee_smoke_alarm.json"),
        Ok(vec![Event::SmokeDetected {
            sensor: "kitchen_smoke".into()
        }])
    );
    assert_eq!(
        parse("zigbee2mqtt/sink_leak", "zigbee_leak.json"),
        Ok(vec![Event::LeakDetected {
            sensor: "sink_leak".into()
        }])
    );
    assert_eq!(
        parse(
            "zigbee2mqtt/front_door/availability",
            "zigbee_availability_offline.json"
        ),
        Ok(vec![Event::DeviceOffline {
            device: "front_door".into()
        }])
    );
    assert_eq!(
        parse("zigbee2mqtt/hall_plug", "zigbee_plug_state.json"),
        Ok(vec![])
    );
}

#[test]
fn frigate_messages() {
    assert_eq!(
        parse("frigate/events", "frigate_event_new_person.json"),
        Ok(vec![Event::PersonDetected {
            camera: "front_cam".into(),
            score: 0.8125,
            snapshot_ref: Some("1793051000.123456-k3x9qa".into())
        }])
    );
    assert_eq!(
        parse("frigate/events", "frigate_event_end.json"),
        Ok(vec![])
    );
    assert_eq!(
        parse("frigate/events", "frigate_event_car.json"),
        Ok(vec![])
    );
    assert_eq!(
        parse("frigate/available", "frigate_available_offline.txt")
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        parse(
            "frigate/garden_cam/status/detect",
            "frigate_status_online.txt"
        ),
        Ok(vec![Event::DeviceOnline {
            device: "garden_cam".into()
        }])
    );
}

#[test]
fn wisense_bridge_messages() {
    assert_eq!(
        parse("homeguard/wisense/wlp2s0", "wisense_motion.json"),
        Ok(vec![Event::MotionDetected {
            source: MotionSource::Wisense,
            sensor: "living_wisense".into()
        }])
    );
    assert_eq!(
        parse("homeguard/wisense/wlp2s0", "wisense_heartbeat.json"),
        Ok(vec![Event::DeviceOnline {
            device: "living_wisense".into()
        }])
    );
}

#[test]
fn garbage_is_rejected_everywhere() {
    let topics = [
        "zigbee2mqtt/front_door",
        "zigbee2mqtt/front_door/availability",
        "frigate/events",
        "frigate/available",
        "frigate/front_cam/status/detect",
        "homeguard/wisense/wlp2s0",
    ];
    for name in [
        "malformed_truncated.json",
        "malformed_binary.bin",
        "malformed_array.json",
    ] {
        for topic in topics {
            assert!(parse(topic, name).is_err(), "{topic} accepted {name}");
        }
    }
}

#[test]
fn wrong_shapes_are_rejected_where_they_matter() {
    assert!(parse("zigbee2mqtt/front_door", "malformed_wrong_type.json").is_err());
    assert!(parse("frigate/events", "malformed_frigate_no_after.json").is_err());
    assert!(parse("homeguard/wisense/wlp2s0", "malformed_wrong_type.json").is_err());
    // A well-formed object with no field this topic knows: nothing to do.
    assert_eq!(
        parse("zigbee2mqtt/front_door", "malformed_frigate_no_after.json"),
        Ok(vec![])
    );
}

#[test]
fn every_fixture_on_every_topic_never_panics() {
    let dir = format!("{}/tests/fixtures/mqtt", env!("CARGO_MANIFEST_DIR"));
    let topics = [
        "zigbee2mqtt/x",
        "zigbee2mqtt/x/availability",
        "zigbee2mqtt/bridge",
        "frigate/events",
        "frigate/available",
        "frigate/x/status/detect",
        "homeguard/wisense/x",
        "unrelated/topic",
    ];
    for entry in std::fs::read_dir(dir).unwrap() {
        let payload = std::fs::read(entry.unwrap().path()).unwrap();
        for topic in topics {
            let _ = mapping().parse(topic, &payload);
        }
    }
}
