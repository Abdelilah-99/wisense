//! The pure core: the alarm state machine and everything it decides.
//!
//! Nothing in here reads the clock or performs I/O. Time arrives inside each
//! [`Input`], and decisions leave as [`Action`]s for the shell to carry out.
//! That makes every rule testable with exact timings, and lets the simulator
//! replay scripted timelines through the same code that runs on the hub.

pub mod config;
pub mod engine;
pub mod event;
pub mod machine;
pub mod state;
pub mod time;

pub use config::{
    AlarmConfig, CameraSpec, Catalog, ConfigError, CoreConfig, FusionConfig, PresenceRules,
    SensorKind, SensorSpec,
};
pub use engine::Engine;
pub use event::{Action, AlertLevel, Command, Event, Input, Mode, MotionSource};
pub use machine::{AlarmMachine, Delay};
pub use state::{Cause, State};
pub use time::Ts;
