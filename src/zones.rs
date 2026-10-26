//! Multi-link aggregation: which sensor link is most disturbed right now.
//!
//! This is *not* positioning. With several links (the laptop plus ESP32
//! boards, say) it answers "which link's signal is disturbed the most,
//! relative to its own threshold", which roughly tracks which link path
//! someone is moving through.
//!
//! Each link's score is normalized as `score / threshold`. While any link is
//! in MOTION, the zone is the link with the highest normalized score. A
//! different link must stay on top for the hold time (1 s) before the zone
//! switches, so two similar links don't ping-pong. Pure: time comes from the
//! events.

use std::collections::BTreeMap;

use crate::detector::{Event, MotionState};

#[derive(Debug, Clone, Copy, PartialEq)]
struct LinkView {
    normalized: f32,
    in_motion: bool,
}

/// Turns per-link `Status` events into `ZoneChanged` events.
#[derive(Debug, Clone)]
pub struct ZoneTracker {
    hold_ms: u64,
    links: BTreeMap<String, LinkView>,
    current: Option<String>,
    /// A different top link and since when it has been on top.
    candidate: Option<(String, u64)>,
}

impl ZoneTracker {
    pub fn new(hold_ms: u64) -> Self {
        Self {
            hold_ms,
            links: BTreeMap::new(),
            current: None,
            candidate: None,
        }
    }

    /// Feed every detector event; returns a `ZoneChanged` when the zone
    /// changes. Needs at least two links before it reports anything.
    pub fn observe(&mut self, event: &Event) -> Option<Event> {
        let Event::Status {
            link_id,
            ts_unix_ms,
            score,
            threshold,
            state,
            ..
        } = event
        else {
            return None;
        };
        let normalized = match (score, threshold) {
            (Some(s), Some(t)) if *t > 0.0 => s / t,
            _ => 0.0,
        };
        self.links.insert(
            link_id.clone(),
            LinkView {
                normalized,
                in_motion: *state == MotionState::Motion,
            },
        );
        if self.links.len() < 2 {
            return None;
        }

        let (top, confidence) = self.ranking();
        if top == self.current {
            self.candidate = None;
            return None;
        }
        // Entering or leaving "no zone" follows the (already debounced)
        // MOTION state immediately; switching between zones waits for the hold.
        let switch = match (&self.current, &top, &self.candidate) {
            (None, _, _) | (_, None, _) => true,
            (Some(_), Some(new), Some((cand, since))) if cand == new => {
                ts_unix_ms.saturating_sub(*since) >= self.hold_ms
            }
            (Some(_), Some(new), _) => {
                self.candidate = Some((new.clone(), *ts_unix_ms));
                self.hold_ms == 0
            }
        };
        if !switch {
            return None;
        }
        self.candidate = None;
        self.current.clone_from(&top);
        Some(Event::ZoneChanged {
            ts_unix_ms: *ts_unix_ms,
            zone: top,
            confidence,
        })
    }

    /// The most disturbed link (if any link is in MOTION) and the margin of
    /// its normalized score over the runner-up.
    fn ranking(&self) -> (Option<String>, f32) {
        if !self.links.values().any(|l| l.in_motion) {
            return (None, 0.0);
        }
        let mut ranked: Vec<(&String, f32)> = self
            .links
            .iter()
            .map(|(id, l)| (id, l.normalized))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let top = ranked.first().map(|(id, _)| (*id).clone());
        let confidence = match ranked.as_slice() {
            [first, second, ..] => first.1 - second.1,
            _ => 0.0,
        };
        (top, confidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::ScoreKind;

    fn status(link: &str, ts: u64, score: f32, state: MotionState) -> Event {
        Event::Status {
            link_id: link.into(),
            ts_unix_ms: ts,
            kind: ScoreKind::Csi,
            score: Some(score),
            threshold: Some(1.0),
            state,
        }
    }

    use MotionState::{Idle, Motion};

    fn zone(ev: Option<Event>) -> Option<(Option<String>, f32)> {
        match ev {
            Some(Event::ZoneChanged {
                zone, confidence, ..
            }) => Some((zone, confidence)),
            _ => None,
        }
    }

    #[test]
    fn single_link_never_reports_a_zone() {
        let mut z = ZoneTracker::new(1000);
        assert_eq!(z.observe(&status("kitchen", 0, 5.0, Motion)), None);
        assert_eq!(z.observe(&status("kitchen", 500, 5.0, Motion)), None);
    }

    #[test]
    fn zone_follows_most_disturbed_link_with_hold() {
        let mut z = ZoneTracker::new(1000);
        assert_eq!(z.observe(&status("kitchen", 0, 0.5, Idle)), None);
        assert_eq!(z.observe(&status("hallway", 0, 0.4, Idle)), None);

        // Motion in the kitchen: zone appears immediately.
        let (zone_now, conf) = zone(z.observe(&status("kitchen", 500, 3.0, Motion))).unwrap();
        assert_eq!(zone_now.as_deref(), Some("kitchen"));
        assert!((conf - 2.6).abs() < 1e-6);

        // Hallway overtakes, but only switches once it has led for 1 s.
        assert_eq!(z.observe(&status("hallway", 1000, 4.0, Motion)), None);
        assert_eq!(z.observe(&status("kitchen", 1500, 2.0, Motion)), None);
        assert_eq!(z.observe(&status("hallway", 1900, 4.0, Motion)), None);
        let (zone_now, _) = zone(z.observe(&status("kitchen", 2000, 2.0, Motion))).unwrap();
        assert_eq!(zone_now.as_deref(), Some("hallway"));

        // Everything calms down: the zone clears right away.
        assert_eq!(z.observe(&status("hallway", 2500, 0.3, Idle)), None);
        let (zone_now, conf) = zone(z.observe(&status("kitchen", 3000, 0.2, Idle))).unwrap();
        assert_eq!(zone_now, None);
        assert_eq!(conf, 0.0);
    }

    #[test]
    fn brief_lead_does_not_ping_pong() {
        let mut z = ZoneTracker::new(1000);
        assert_eq!(z.observe(&status("a", 0, 3.0, Motion)), None);
        let (first, _) = zone(z.observe(&status("b", 0, 2.0, Motion))).unwrap();
        assert_eq!(first.as_deref(), Some("a"));
        let mut changes = Vec::new();
        // b and a alternate the lead every 500 ms for 10 s.
        for i in 0..20u64 {
            let ts = 500 + i * 500;
            let (lead, lag) = if i % 2 == 0 { ("b", "a") } else { ("a", "b") };
            changes.extend(zone(z.observe(&status(lead, ts, 3.0, Motion))));
            changes.extend(zone(z.observe(&status(lag, ts, 2.0, Motion))));
        }
        assert!(changes.is_empty(), "{changes:?}");
    }
}
