//! Dashboard authentication helpers. Pure.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;

use crate::core::Ts;

/// Compares without stopping at the first difference, so response times do
/// not reveal how much of a guess was right. (The length is not secret.)
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Checks an `Authorization: Basic ...` header against the dashboard
/// password. Any user name is accepted; only the password matters.
pub fn basic_auth_ok(header: Option<&str>, password: &str) -> bool {
    let Some(encoded) = header.and_then(|h| h.strip_prefix("Basic ")) else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&decoded) else {
        return false;
    };
    let Some((_user, given)) = text.split_once(':') else {
        return false;
    };
    constant_time_eq(given.as_bytes(), password.as_bytes())
}

/// Locks a client out after too many failures (wrong password or PIN), to
/// make guessing impractical.
#[derive(Debug, Clone)]
pub struct Throttle {
    max_failures: usize,
    window: Duration,
    lockout: Duration,
    clients: HashMap<String, Client>,
}

#[derive(Debug, Clone, Default)]
struct Client {
    failures: Vec<Ts>,
    locked_until: Option<Ts>,
}

impl Throttle {
    /// `max_failures` within `window` locks the client out for `lockout`.
    pub fn new(max_failures: usize, window: Duration, lockout: Duration) -> Self {
        Throttle {
            max_failures,
            window,
            lockout,
            clients: HashMap::new(),
        }
    }

    /// Whether `client` may try now.
    pub fn allowed(&mut self, client: &str, now: Ts) -> bool {
        match self.clients.get_mut(client) {
            Some(c) => match c.locked_until {
                Some(until) if now < until => false,
                Some(_) => {
                    c.locked_until = None;
                    c.failures.clear();
                    true
                }
                None => true,
            },
            None => true,
        }
    }

    pub fn failure(&mut self, client: &str, now: Ts) {
        let window = self.window;
        let entry = self.clients.entry(client.to_string()).or_default();
        entry.failures.retain(|t| now.since(*t) < window);
        entry.failures.push(now);
        if entry.failures.len() >= self.max_failures {
            entry.locked_until = Some(now + self.lockout);
        }
        // Bound memory if many clients fail.
        if self.clients.len() > 1024 {
            self.clients
                .retain(|_, c| c.locked_until.is_some() || !c.failures.is_empty());
        }
    }

    pub fn success(&mut self, client: &str) {
        self.clients.remove(client);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(user_pass: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(user_pass)
        )
    }

    #[test]
    fn basic_auth() {
        assert!(basic_auth_ok(Some(&header("admin:s3cret")), "s3cret"));
        assert!(basic_auth_ok(Some(&header(":s3cret")), "s3cret"));
        assert!(basic_auth_ok(Some(&header("me:pa:ss")), "pa:ss"));
        assert!(!basic_auth_ok(Some(&header("admin:wrong")), "s3cret"));
        assert!(!basic_auth_ok(Some(&header("admin")), "s3cret"));
        assert!(!basic_auth_ok(Some("Basic !!!"), "s3cret"));
        assert!(!basic_auth_ok(Some("Bearer abc"), "s3cret"));
        assert!(!basic_auth_ok(None, "s3cret"));
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"1234", b"1234"));
        assert!(!constant_time_eq(b"1234", b"1235"));
        assert!(!constant_time_eq(b"1234", b"12345"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn throttle_locks_out_and_recovers() {
        let mut t = Throttle::new(3, Duration::from_secs(60), Duration::from_secs(300));
        let at = Ts::from_secs;
        assert!(t.allowed("a", at(0)));
        t.failure("a", at(0));
        t.failure("a", at(10));
        assert!(t.allowed("a", at(11)));
        t.failure("a", at(20));
        assert!(!t.allowed("a", at(21)));
        // Other clients are not affected.
        assert!(t.allowed("b", at(21)));
        assert!(!t.allowed("a", at(319)));
        assert!(t.allowed("a", at(320)));
        // The slate is clean after a lockout.
        t.failure("a", at(321));
        assert!(t.allowed("a", at(322)));
    }

    #[test]
    fn throttle_window_and_success() {
        let mut t = Throttle::new(3, Duration::from_secs(60), Duration::from_secs(300));
        let at = Ts::from_secs;
        t.failure("a", at(0));
        t.failure("a", at(30));
        // The first failure left the window.
        t.failure("a", at(61));
        assert!(t.allowed("a", at(62)));
        t.failure("a", at(70));
        t.success("a");
        t.failure("a", at(71));
        assert!(t.allowed("a", at(72)));
    }
}
