//! Single owner for active turn identity, abort tokens and bounded pre-start cancellation.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const MAX_PENDING_CANCELLATIONS: usize = 1024;
const PENDING_CANCELLATION_TTL: Duration = Duration::from_secs(5 * 60);

struct ActiveTurn {
    id: String,
    abort: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct TurnControls {
    active: HashMap<String, ActiveTurn>,
    pending: HashMap<(String, String), Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CancelDisposition {
    Requested,
    Queued,
    NoActiveTurn,
}

impl TurnControls {
    fn prune(&mut self) {
        let now = Instant::now();
        self.pending.retain(|_, expiry| *expiry > now);
    }

    pub fn register(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Arc<AtomicBool>, &'static str> {
        self.prune();
        if self.active.contains_key(session_id) {
            return Err("turn/already_running");
        }
        let cancelled = self
            .pending
            .remove(&(session_id.into(), turn_id.into()))
            .is_some();
        let abort = Arc::new(AtomicBool::new(cancelled));
        self.active.insert(
            session_id.into(),
            ActiveTurn {
                id: turn_id.into(),
                abort: Arc::clone(&abort),
            },
        );
        Ok(abort)
    }

    pub fn active_id(&self, session_id: &str) -> Option<String> {
        self.active.get(session_id).map(|turn| turn.id.clone())
    }

    pub fn finish(&mut self, session_id: &str, turn_id: &str) {
        if self
            .active
            .get(session_id)
            .is_some_and(|turn| turn.id == turn_id)
        {
            self.active.remove(session_id);
        }
        self.pending.remove(&(session_id.into(), turn_id.into()));
    }

    pub fn cancel(
        &mut self,
        session_id: &str,
        expected_turn_id: Option<&str>,
    ) -> Result<CancelDisposition, &'static str> {
        self.prune();
        if let Some(active) = self.active.get(session_id) {
            if expected_turn_id.is_none_or(|expected| expected == active.id) {
                active.abort.store(true, Ordering::Release);
                return Ok(CancelDisposition::Requested);
            }
        }
        let Some(expected) = expected_turn_id else {
            return Ok(CancelDisposition::NoActiveTurn);
        };
        let key = (session_id.into(), expected.into());
        if !self.pending.contains_key(&key) && self.pending.len() >= MAX_PENDING_CANCELLATIONS {
            return Err("turn/cancellation_capacity");
        }
        self.pending
            .insert(key, Instant::now() + PENDING_CANCELLATION_TTL);
        Ok(CancelDisposition::Queued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prestart_cancel_is_not_cleared_when_the_turn_registers() {
        let mut control = TurnControls::default();
        assert_eq!(
            control.cancel("s", Some("future")).unwrap(),
            CancelDisposition::Queued
        );
        assert!(control
            .register("s", "future")
            .unwrap()
            .load(Ordering::Acquire));
    }
    #[test]
    fn delayed_cancel_and_old_cleanup_do_not_change_the_next_turn() {
        let mut control = TurnControls::default();
        let old = control.register("s", "old").unwrap();
        control.finish("s", "old");
        let next = control.register("s", "next").unwrap();
        assert_eq!(
            control.cancel("s", Some("old")).unwrap(),
            CancelDisposition::Queued
        );
        control.finish("s", "old");
        assert_eq!(control.active_id("s").as_deref(), Some("next"));
        assert!(!next.load(Ordering::Acquire));
        assert!(!old.load(Ordering::Acquire));
    }
    #[test]
    fn cancellation_cache_is_bounded_and_expired_entries_can_be_reused() {
        let mut control = TurnControls::default();
        for index in 0..MAX_PENDING_CANCELLATIONS {
            control.cancel("s", Some(&format!("t-{index}"))).unwrap();
        }
        assert!(control.cancel("s", Some("overflow")).is_err());
        for expiry in control.pending.values_mut() {
            *expiry = Instant::now();
        }
        assert_eq!(
            control.cancel("s", Some("fresh")).unwrap(),
            CancelDisposition::Queued
        );
    }
}
