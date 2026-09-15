use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Wayland,
    X11,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopSession {
    pub id: String,
    pub uid: u32,
    pub kind: SessionKind,
    pub locked: bool,
}

impl DesktopSession {
    pub fn matches_worker(&self, uid: u32, session_id: &str) -> bool {
        self.uid == uid && self.id == session_id && self.kind != SessionKind::Unknown
    }

    pub fn allows_capture(&self, uid: u32, session_id: &str) -> bool {
        !self.locked && self.matches_worker(uid, session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_requires_the_selected_user_session_and_an_unlocked_desktop() {
        let mut session = DesktopSession {
            id: "2".into(),
            uid: 1000,
            kind: SessionKind::Wayland,
            locked: false,
        };
        assert!(session.allows_capture(1000, "2"));
        assert!(!session.allows_capture(1001, "2"));
        assert!(!session.allows_capture(1000, "3"));
        session.locked = true;
        assert!(session.matches_worker(1000, "2"));
        assert!(!session.allows_capture(1000, "2"));
        session.kind = SessionKind::Unknown;
        assert!(!session.matches_worker(1000, "2"));
    }
}
