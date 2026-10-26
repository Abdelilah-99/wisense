//! The committed example configuration must stay valid.

use homeguard::core::{CoreConfig, Engine, Mode};

#[test]
fn example_config_builds_an_engine() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/config/homeguard.toml"
    ))
    .unwrap();
    let config: CoreConfig = toml::from_str(&text).unwrap();
    let engine = Engine::new(&config).unwrap();
    let catalog = engine.catalog();
    assert!(catalog.is_active("front_door", Mode::Home));
    assert!(catalog.is_active("hallway_pir", Mode::Night));
    assert!(!catalog.is_active("living_room_pir", Mode::Night));
    assert_eq!(catalog.zone_of("front_cam"), Some("entrance"));
}
