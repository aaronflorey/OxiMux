//! A terminal agent's keep-awake stake, capped so a wedged status cannot pin
//! the machine awake forever.
//!
//! Once a hook-driven agent has reported `Running`, nothing idle-decays it: a
//! missed Stop hook leaves the tab reading Running indefinitely, and the
//! machine never idle-sleeps again. The cap drops the hold after
//! [`AGENT_AWAKE_STALE_AFTER`] of silence — but silence on **both** the status
//! stream and the tab's PTY output. The status stream alone is not a liveness
//! signal: a hook agent emits nothing between the start and end of one long
//! tool call, and a regex agent emits only on transitions, so a status-only cap
//! would let the machine sleep in the middle of exactly the overnight run this
//! exists for. A working TUI redraws (spinner, timer, streamed tool output); a
//! wedged-but-idle prompt does not.
//!
//! Pure over an explicit `now`, so the policy is testable without GPUI.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent_awake::{AgentAwake, AwakeHold};

/// How long a Running agent may be silent — no status event and no PTY
/// output — before its keep-awake hold is dropped.
pub const AGENT_AWAKE_STALE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);

/// After an expiry, how often the tab's output is checked for signs the agent
/// is working again. A hook agent can resume printing without a new status
/// event, so waiting on the status stream alone would never re-acquire.
pub const AGENT_AWAKE_OUTPUT_RECHECK: Duration = Duration::from_secs(60);

pub struct AgentHoldLease {
    owner: Arc<AgentAwake>,
    hold: Option<AwakeHold>,
    last_event: Instant,
    /// When the cap last dropped the hold while the status still read Running;
    /// cleared by the next status event.
    expired_at: Option<Instant>,
}

impl AgentHoldLease {
    pub fn new(owner: Arc<AgentAwake>, now: Instant) -> Self {
        Self { owner, hold: None, last_event: now, expired_at: None }
    }

    /// A status event: hold while `running`, release otherwise. Any event —
    /// even a repeat of Running — counts as liveness and re-arms the cap, and
    /// a Running event after an expiry re-acquires.
    pub fn observe(&mut self, running: bool, now: Instant) {
        self.last_event = now;
        self.expired_at = None;
        match (&self.hold, running) {
            (None, true) => self.hold = Some(self.owner.acquire()),
            (Some(_), false) => self.hold = None,
            _ => {}
        }
    }

    /// The staleness timer fired. Returns how long to wait before checking
    /// again when the agent is still live (output or an event arrived within
    /// the cap), or `None` after dropping the hold.
    pub fn on_deadline(&mut self, last_output: Option<Instant>, now: Instant) -> Option<Duration> {
        self.hold.as_ref()?;
        let last_alive = last_output.map_or(self.last_event, |o| o.max(self.last_event));
        let deadline = last_alive + AGENT_AWAKE_STALE_AFTER;
        if deadline > now {
            return Some(deadline - now);
        }
        self.hold = None;
        self.expired_at = Some(now);
        None
    }

    /// After an expiry, PTY output newer than it means the agent is working
    /// again: re-acquire. Returns whether it did.
    pub fn on_output(&mut self, last_output: Option<Instant>) -> bool {
        let Some(expired_at) = self.expired_at else {
            return false;
        };
        if last_output.is_none_or(|o| o <= expired_at) {
            return false;
        }
        self.expired_at = None;
        self.hold = Some(self.owner.acquire());
        true
    }

    /// The cap dropped the hold and no status event has arrived since.
    pub fn expired(&self) -> bool {
        self.expired_at.is_some()
    }

    pub fn holding(&self) -> bool {
        self.hold.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_awake::testing::fixture;

    const CAP: Duration = AGENT_AWAKE_STALE_AFTER;
    const MIN: Duration = Duration::from_secs(60);

    fn lease() -> (AgentHoldLease, Arc<AgentAwake>, Instant) {
        let (awake, _backend) = fixture(true);
        let t0 = Instant::now();
        (AgentHoldLease::new(awake.clone(), t0), awake, t0)
    }

    #[test]
    fn running_holds() {
        let (mut lease, awake, t0) = lease();
        lease.observe(true, t0);
        assert!(lease.holding());
        assert_eq!(awake.status().agents, 1);
        assert!(awake.status().asserted);
    }

    #[test]
    fn observe_idle_releases() {
        let (mut lease, awake, t0) = lease();
        lease.observe(true, t0);
        lease.observe(false, t0 + MIN);
        assert!(!lease.holding());
        assert_eq!(awake.status().agents, 0);
    }

    #[test]
    fn deadline_with_no_output_releases() {
        let (mut lease, awake, t0) = lease();
        lease.observe(true, t0);
        assert_eq!(lease.on_deadline(None, t0 + CAP), None);
        assert!(!lease.holding());
        assert_eq!(awake.status().agents, 0);
        assert!(!awake.status().asserted);
    }

    #[test]
    fn deadline_with_stale_output_releases() {
        let (mut lease, _awake, t0) = lease();
        lease.observe(true, t0 + MIN);
        assert_eq!(lease.on_deadline(Some(t0), t0 + MIN + CAP), None);
        assert!(!lease.holding());
    }

    /// A long, status-silent tool call that keeps printing keeps its hold.
    #[test]
    fn deadline_with_recent_output_rearms_for_remainder() {
        let (mut lease, awake, t0) = lease();
        lease.observe(true, t0);
        let output = t0 + 30 * MIN;
        assert_eq!(lease.on_deadline(Some(output), t0 + CAP), Some(30 * MIN));
        assert!(lease.holding());
        assert_eq!(awake.status().agents, 1);
    }

    #[test]
    fn a_status_event_rearms_the_cap() {
        let (mut lease, _awake, t0) = lease();
        lease.observe(true, t0);
        lease.observe(true, t0 + 10 * MIN);
        assert_eq!(lease.on_deadline(None, t0 + CAP), Some(10 * MIN));
    }

    #[test]
    fn observe_running_after_expiry_reacquires() {
        let (mut lease, awake, t0) = lease();
        lease.observe(true, t0);
        assert_eq!(lease.on_deadline(None, t0 + CAP), None);
        lease.observe(true, t0 + CAP + MIN);
        assert!(lease.holding());
        assert_eq!(awake.status().agents, 1);
    }

    /// A hook agent resuming output without a status event gets its hold back.
    #[test]
    fn output_after_expiry_reacquires() {
        let (mut lease, awake, t0) = lease();
        lease.observe(true, t0);
        let expiry = t0 + CAP;
        assert_eq!(lease.on_deadline(None, expiry), None);
        assert!(lease.expired());
        assert!(!lease.on_output(Some(expiry - MIN)), "output from before the expiry is stale");
        assert!(!lease.on_output(None));
        assert!(lease.on_output(Some(expiry + MIN)));
        assert!(lease.holding() && !lease.expired());
        assert_eq!(awake.status().agents, 1);
    }

    #[test]
    fn a_status_event_ends_the_expired_state() {
        let (mut lease, _awake, t0) = lease();
        lease.observe(true, t0);
        lease.on_deadline(None, t0 + CAP);
        lease.observe(false, t0 + CAP + MIN);
        assert!(!lease.expired());
        assert!(!lease.on_output(Some(t0 + CAP + 2 * MIN)), "idle now: output does not hold");
    }

    #[test]
    fn deadline_while_not_holding_is_a_no_op() {
        let (mut lease, _awake, t0) = lease();
        assert_eq!(lease.on_deadline(None, t0 + CAP), None);
        assert!(!lease.holding());
    }

    /// A tab closed mid-run drops its task, and with it the lease.
    #[test]
    fn drop_while_holding_releases() {
        let (mut lease, awake, t0) = lease();
        lease.observe(true, t0);
        drop(lease);
        assert_eq!(awake.status().agents, 0);
        assert!(!awake.status().asserted);
    }
}
