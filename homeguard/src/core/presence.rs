//! Who is home, from phone sightings on the LAN.
//!
//! A phone is "home" from its first sighting. It becomes "away" only after
//! it has not been seen for the grace period (default 10 minutes), because
//! phones sleep and stop answering for minutes at a time while their owner
//! is on the sofa. A phone never seen since startup counts as unseen since
//! startup, so after a reboot with nobody home everyone is "away" once the
//! grace period has passed.

use std::collections::BTreeMap;
use std::time::Duration;

use super::event::Event;
use super::time::Ts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Person {
    home: Option<bool>,
    last_seen: Option<Ts>,
}

#[derive(Debug, Clone)]
pub struct PresenceTracker {
    grace: Duration,
    started: Ts,
    people: BTreeMap<String, Person>,
}

impl PresenceTracker {
    pub fn new(people: impl IntoIterator<Item = String>, grace: Duration, started: Ts) -> Self {
        PresenceTracker {
            grace,
            started,
            people: people
                .into_iter()
                .map(|name| {
                    (
                        name,
                        Person {
                            home: None,
                            last_seen: None,
                        },
                    )
                })
                .collect(),
        }
    }

    /// The phone of `person` answered at `at`.
    pub fn sighting(&mut self, at: Ts, person: &str) -> Vec<Event> {
        let Some(state) = self.people.get_mut(person) else {
            return Vec::new();
        };
        state.last_seen = Some(state.last_seen.map_or(at, |last| last.max(at)));
        if state.home == Some(true) {
            return Vec::new();
        }
        state.home = Some(true);
        vec![Event::PresenceChanged {
            person: person.to_string(),
            home: true,
        }]
    }

    /// Marks away everyone unseen for the grace period.
    pub fn tick(&mut self, at: Ts) -> Vec<Event> {
        let mut events = Vec::new();
        for (name, state) in &mut self.people {
            let since = state.last_seen.unwrap_or(self.started);
            if state.home != Some(false) && at.since(since) >= self.grace {
                state.home = Some(false);
                events.push(Event::PresenceChanged {
                    person: name.clone(),
                    home: false,
                });
            }
        }
        events
    }

    /// When the next person would be marked away.
    pub fn next_deadline(&self) -> Option<Ts> {
        self.people
            .values()
            .filter(|p| p.home != Some(false))
            .map(|p| p.last_seen.unwrap_or(self.started) + self.grace)
            .min()
    }

    pub fn is_home(&self, person: &str) -> Option<bool> {
        self.people.get(person).and_then(|p| p.home)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = Duration::from_secs(600);

    fn tracker() -> PresenceTracker {
        PresenceTracker::new(["alice".to_string(), "bob".to_string()], GRACE, Ts::ZERO)
    }

    fn changed(person: &str, home: bool) -> Event {
        Event::PresenceChanged {
            person: person.into(),
            home,
        }
    }

    #[test]
    fn home_on_first_sighting_only() {
        let mut t = tracker();
        assert_eq!(
            t.sighting(Ts::from_secs(5), "alice"),
            vec![changed("alice", true)]
        );
        assert!(t.sighting(Ts::from_secs(35), "alice").is_empty());
        assert_eq!(t.is_home("alice"), Some(true));
        assert_eq!(t.is_home("bob"), None);
    }

    #[test]
    fn away_only_after_the_grace_period() {
        let mut t = tracker();
        t.sighting(Ts::from_secs(100), "alice");
        // The phone sleeps: no answer for 9 min 59 s.
        assert!(t
            .tick(Ts::from_secs(699))
            .iter()
            .all(|e| *e != changed("alice", false)));
        assert_eq!(t.tick(Ts::from_secs(700)), vec![changed("alice", false)]);
        assert!(t.tick(Ts::from_secs(800)).is_empty());
        // Back home.
        assert_eq!(
            t.sighting(Ts::from_secs(900), "alice"),
            vec![changed("alice", true)]
        );
    }

    #[test]
    fn a_sleeping_phone_seen_now_and_then_stays_home() {
        let mut t = tracker();
        // Seen every 8 minutes (asleep in between): never away.
        for minute in (0..120).step_by(8) {
            let at = Ts::from_secs(minute * 60);
            t.sighting(at, "alice");
            assert!(t
                .tick(at + Duration::from_secs(7 * 60))
                .iter()
                .all(|e| *e != changed("alice", false)));
        }
        assert_eq!(t.is_home("alice"), Some(true));
    }

    #[test]
    fn never_seen_counts_from_startup() {
        let mut t = PresenceTracker::new(["alice".to_string()], GRACE, Ts::from_secs(50));
        assert_eq!(t.next_deadline(), Some(Ts::from_secs(650)));
        assert!(t.tick(Ts::from_secs(649)).is_empty());
        assert_eq!(t.tick(Ts::from_secs(650)), vec![changed("alice", false)]);
        assert_eq!(t.next_deadline(), None);
    }

    #[test]
    fn unknown_people_and_out_of_order_sightings() {
        let mut t = tracker();
        assert!(t.sighting(Ts::from_secs(1), "mallory").is_empty());
        t.sighting(Ts::from_secs(500), "alice");
        // A late, older sighting does not move last_seen backwards.
        t.sighting(Ts::from_secs(100), "alice");
        assert!(t
            .tick(Ts::from_secs(1099))
            .iter()
            .all(|e| *e != changed("alice", false)));
        assert_eq!(t.tick(Ts::from_secs(1100)), vec![changed("alice", false)]);
    }
}
