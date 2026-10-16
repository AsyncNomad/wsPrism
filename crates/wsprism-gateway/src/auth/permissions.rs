//! Per-user room permissions: subscribe (join, receive) and publish (send)
//! are separate (ADR 0006 §5, ADR 0008 §2). The tenant allowlist says which
//! `svc:type` exist for everyone; these say what one user may do with rooms.

/// A room name pattern: exact (`lobby`) or prefix with a trailing `*`
/// (`match:*`, or `*` for every room).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomPattern {
    text: String,
    prefix: bool,
}

impl RoomPattern {
    pub fn parse(s: &str) -> Self {
        match s.strip_suffix('*') {
            Some(prefix) => Self {
                text: prefix.to_string(),
                prefix: true,
            },
            None => Self {
                text: s.to_string(),
                prefix: false,
            },
        }
    }

    pub fn matches(&self, room: &str) -> bool {
        if self.prefix {
            room.starts_with(&self.text)
        } else {
            room == self.text
        }
    }
}

/// `None` = unrestricted (what development and plain tickets get).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    pub subscribe: Option<Vec<RoomPattern>>,
    pub publish: Option<Vec<RoomPattern>>,
}

impl Permissions {
    /// Restricted permissions from pattern strings (e.g. token claims).
    pub fn restricted(subscribe: &[&str], publish: &[&str]) -> Self {
        let parse = |v: &[&str]| Some(v.iter().map(|p| RoomPattern::parse(p)).collect());
        Self {
            subscribe: parse(subscribe),
            publish: parse(publish),
        }
    }

    pub fn can_subscribe(&self, room: &str) -> bool {
        allows(&self.subscribe, room)
    }

    pub fn can_publish(&self, room: &str) -> bool {
        allows(&self.publish, room)
    }
}

fn allows(rule: &Option<Vec<RoomPattern>>, room: &str) -> bool {
    rule.as_ref()
        .is_none_or(|patterns| patterns.iter().any(|p| p.matches(room)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_allows_everything() {
        let p = Permissions::default();
        assert!(p.can_subscribe("any") && p.can_publish("any"));
    }

    #[test]
    fn exact_and_prefix_patterns() {
        let p = Permissions::restricted(&["lobby", "match:*"], &["match:7"]);
        assert!(p.can_subscribe("lobby"));
        assert!(!p.can_subscribe("lobby2"), "exact means exact");
        assert!(p.can_subscribe("match:anything"));
        assert!(p.can_publish("match:7"));
        assert!(!p.can_publish("match:8"));
        assert!(!p.can_publish("lobby"), "subscribe does not imply publish");
    }

    #[test]
    fn spectator_can_watch_but_not_send() {
        let p = Permissions::restricted(&["*"], &[]);
        assert!(p.can_subscribe("match:1"));
        assert!(!p.can_publish("match:1"));
    }
}
