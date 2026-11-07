//! A device operation starts only after its busy claim is committed.
use crate::error::{Result, TapeError};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceActivityState {
    pub node: u64,
    pub instance: u64,
    pub round: u64,
    pub sequence: u64,
    pub busy: bool,
}
#[derive(Debug, Clone, Copy)]
pub(in crate::daemon) struct ActivityUpdate {
    pub round: u64,
    pub sequence: u64,
    pub busy: bool,
}
#[derive(Debug, Default)]
struct State {
    enabled: bool,
    node: u64,
    instance: u64,
    round: u64,
    sequence: u64,
    depth: usize,
    closed: bool,
    acknowledged: Option<DeviceActivityState>,
    pending: VecDeque<ActivityUpdate>,
}
#[derive(Debug, Default)]
pub(super) struct ActivityRegistry {
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Debug)]
pub struct DeviceActivityGuard<'a> {
    registry: &'a ActivityRegistry,
    round: u64,
    tracked: bool,
}
impl ActivityRegistry {
    pub(super) fn recovering(&self, round: u64) -> DeviceActivityGuard<'_> {
        DeviceActivityGuard {
            registry: self,
            round,
            tracked: false,
        }
    }
    pub(super) fn enable(&self, node: u64, instance: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(node > 0 && instance > 0 && !s.enabled);
        s.enabled = true;
        s.node = node;
        s.instance = instance;
    }
    pub(super) fn serving(&self, round: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.enabled || s.round == round {
            return;
        }
        s.round = round;
        s.sequence = 1;
        s.depth = 0;
        s.closed = false;
        s.acknowledged = None;
        s.pending.clear();
        s.pending.push_back(ActivityUpdate {
            round,
            sequence: 1,
            busy: false,
        });
        self.changed.notify_all();
    }
    pub(super) fn close(&self, round: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.round == round {
            s.closed = true;
            s.pending.clear();
            self.changed.notify_all();
        }
    }
    pub(super) fn next(&self) -> Option<ActivityUpdate> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .pop_front()
    }
    pub(super) fn observe(&self, proof: Option<&DeviceActivityState>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = proof.filter(|p| {
            s.enabled
                && !s.closed
                && p.node == s.node
                && p.instance == s.instance
                && p.round == s.round
                && p.sequence > 0
                && p.sequence <= s.sequence
        }) && s
            .acknowledged
            .as_ref()
            .is_none_or(|old| p.sequence >= old.sequence)
        {
            s.acknowledged = Some(p.clone());
            self.changed.notify_all();
        }
    }
    pub(super) fn begin(
        &self,
        round: u64,
        deadline: Instant,
        valid: impl Fn() -> bool,
    ) -> Result<DeviceActivityGuard<'_>> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.enabled {
            return Ok(DeviceActivityGuard {
                registry: self,
                round,
                tracked: false,
            });
        }
        if s.closed || s.round != round {
            return Err(lost());
        }
        if s.depth == 0 {
            s.sequence = s.sequence.checked_add(1).ok_or_else(lost)?;
            let sequence = s.sequence;
            s.pending.push_back(ActivityUpdate {
                round,
                sequence,
                busy: true,
            });
        }
        s.depth = s.depth.checked_add(1).ok_or_else(lost)?;
        let wanted = s.sequence;
        let guard = DeviceActivityGuard {
            registry: self,
            round,
            tracked: true,
        };
        loop {
            if s.closed || s.round != round || !valid() || Instant::now() >= deadline {
                drop(s);
                drop(guard);
                return Err(lost());
            }
            if s.acknowledged
                .as_ref()
                .is_some_and(|p| p.round == round && p.sequence == wanted && p.busy)
            {
                return Ok(guard);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            s = self
                .changed
                .wait_timeout(s, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
fn lost() -> TapeError {
    TapeError::OwnershipLost {
        device: "library".into(),
        detail: "设备忙状态尚未获得当前轮次的多数派确认".into(),
    }
}
impl Drop for DeviceActivityGuard<'_> {
    fn drop(&mut self) {
        if !self.tracked {
            return;
        }
        let mut s = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if s.closed || s.round != self.round || s.depth == 0 {
            return;
        }
        s.depth -= 1;
        if s.depth == 0 {
            if let Some(sequence) = s.sequence.checked_add(1) {
                s.sequence = sequence;
                s.pending.push_back(ActivityUpdate {
                    round: self.round,
                    sequence,
                    busy: false,
                });
            } else {
                s.closed = true;
            }
        }
        self.registry.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    fn proof(sequence: u64, busy: bool) -> DeviceActivityState {
        DeviceActivityState {
            node: 1,
            instance: 11,
            round: 3,
            sequence,
            busy,
        }
    }
    fn next(registry: &ActivityRegistry) -> ActivityUpdate {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(update) = registry.next() {
                return update;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    #[test]
    fn actual_io_waits_for_exact_busy_commit_and_nested_work_stays_busy() {
        let registry = ActivityRegistry::default();
        registry.enable(1, 11);
        registry.serving(3);
        assert!(!next(&registry).busy);
        registry.observe(Some(&proof(1, false)));
        let (started, rx) = mpsc::channel();
        let (release, done) = mpsc::channel();
        std::thread::scope(|scope| {
            let registry = &registry;
            scope.spawn(move || {
                let outer = registry
                    .begin(3, Instant::now() + Duration::from_secs(2), || true)
                    .unwrap();
                let inner = registry
                    .begin(3, Instant::now() + Duration::from_secs(2), || true)
                    .unwrap();
                drop(inner);
                started.send(()).unwrap();
                done.recv().unwrap();
                drop(outer);
            });
            let update = next(registry);
            assert_eq!((update.sequence, update.busy), (2, true));
            assert!(
                rx.try_recv().is_err(),
                "no I/O before consensus acknowledgement"
            );
            let mut wrong = proof(2, true);
            wrong.instance = 12;
            registry.observe(Some(&wrong));
            registry.observe(Some(&proof(1, false)));
            assert!(rx.try_recv().is_err());
            registry.observe(Some(&proof(2, true)));
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(
                registry.next().is_none(),
                "nested guard must not publish idle"
            );
            release.send(()).unwrap();
        });
        let update = next(&registry);
        assert_eq!((update.sequence, update.busy), (3, false));
        assert!(registry.next().is_none());
    }
    #[test]
    fn ownership_loss_wakes_a_waiter_without_starting_io() {
        let registry = ActivityRegistry::default();
        registry.enable(1, 11);
        registry.serving(3);
        next(&registry);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                registry
                    .begin(3, Instant::now() + Duration::from_secs(2), || true)
                    .unwrap_err()
            });
            assert!(next(&registry).busy);
            registry.close(3);
            assert!(matches!(
                worker.join().unwrap(),
                TapeError::OwnershipLost { .. }
            ));
        });
        assert!(registry.next().is_none());
    }
}
