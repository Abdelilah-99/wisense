//! Runs every scenario in scenarios/ through the real core and checks the
//! expectations each one declares.

use std::path::{Path, PathBuf};

use homeguard::sim;

fn scenario_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_scenario_meets_its_expectations() {
    let files = scenario_files();
    assert!(!files.is_empty());
    let mut failed = Vec::new();
    for path in &files {
        let (scenario, config) = sim::load(path).unwrap();
        let expect = scenario
            .expect
            .clone()
            .unwrap_or_else(|| panic!("{} has no [expect] section", path.display()));
        let outcome = sim::simulate(&config, &scenario).unwrap();
        for failure in sim::check(&outcome, &expect) {
            failed.push(format!("{}: {failure}", path.display()));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn the_required_scenarios_exist() {
    let names: Vec<String> = scenario_files()
        .iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    for required in [
        "burglar_front_door",
        "motion_without_person",
        "owner_coming_home",
        "camera_unplugged_while_armed",
        "smoke_while_disarmed",
        "flaky_motion_flood",
    ] {
        assert!(names.iter().any(|n| n == required), "missing {required}");
    }
}

#[test]
fn simulation_is_deterministic() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios/flaky_motion_flood.toml");
    let (scenario, config) = sim::load(&path).unwrap();
    let a = sim::simulate(&config, &scenario).unwrap();
    let b = sim::simulate(&config, &scenario).unwrap();
    assert_eq!(a.records, b.records);
}
