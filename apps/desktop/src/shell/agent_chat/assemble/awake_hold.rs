//! Agent Chat's keep-awake stake: each chat view holds one agent
//! [`AwakeHold`] exactly while a turn is in flight on a live connection, so
//! Agent mode keeps the machine awake for a chat turn the same way it does for
//! a Running terminal agent.
//!
//! A side table keyed by the view's entity rather than a field, because the
//! view struct lives in a file that may not grow. Driven from `sync_composer`,
//! which already runs on every turn-active and connection flip; the entry is
//! dropped when the view is released, so a tab closed mid-turn lets go too.
//!
//! No staleness cap, unlike terminal agents: `turn_active` comes from the
//! protocol, and a dead process marks the view disconnected, which releases.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::ThreadId;

use gpui::{Context, EntityId};

use crate::agent_awake::{self, AgentAwake, AwakeHold};
use crate::shell::agent_chat::AgentChatView;

/// Presence of a key means the view's release hook is registered; the value
/// is its hold, if a turn is in flight.
struct Registry<K> {
    slots: HashMap<K, Option<AwakeHold>>,
}

impl<K: Hash + Eq> Registry<K> {
    fn new() -> Self {
        Self { slots: HashMap::new() }
    }

    /// Reconcile `key`'s hold with `active`. Returns `true` the first time a
    /// key is seen, so the caller registers its release hook exactly once.
    fn sync(&mut self, key: K, active: bool, owner: &Arc<AgentAwake>) -> bool {
        let first = !self.slots.contains_key(&key);
        reconcile(self.slots.entry(key).or_default(), active, owner);
        first
    }

    fn release(&mut self, key: &K) {
        self.slots.remove(key);
    }
}

/// Keyed by the app's thread as well as the entity: entity ids are only unique
/// within one `App`, and parallel GPUI tests each run their own on their own
/// thread. The app itself has one.
type Key = (ThreadId, EntityId);

static HOLDS: LazyLock<Mutex<Registry<Key>>> = LazyLock::new(|| Mutex::new(Registry::new()));

fn holds() -> std::sync::MutexGuard<'static, Registry<Key>> {
    HOLDS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Hold while `active`, release otherwise; a repeat is a no-op.
fn reconcile(slot: &mut Option<AwakeHold>, active: bool, owner: &Arc<AgentAwake>) {
    match (slot.is_some(), active) {
        (false, true) => *slot = Some(owner.acquire()),
        (true, false) => *slot = None,
        _ => {}
    }
}

/// Called from `sync_composer` with whether a turn is working on a live
/// connection.
pub(super) fn sync(active: bool, cx: &mut Context<AgentChatView>) {
    sync_to(agent_awake::global(), active, cx);
}

/// [`sync`] over an injected holder, so a test can observe the effect.
fn sync_to<T: 'static>(owner: &Arc<AgentAwake>, active: bool, cx: &mut Context<T>) {
    let key = (std::thread::current().id(), cx.entity_id());
    let first = holds().sync(key, active, owner);
    if first {
        cx.on_release(move |_, _| holds().release(&key)).detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_awake::testing::fixture;

    #[test]
    fn a_turn_holds_and_its_end_releases() {
        let (awake, backend) = fixture(true);
        let mut registry = Registry::new();
        assert!(registry.sync(1, true, &awake), "first sighting registers the release hook");
        assert!(awake.status().asserted);
        assert!(!registry.sync(1, true, &awake), "a repeat is not a first sighting");
        assert_eq!(awake.status().agents, 1, "a repeat does not stack holds");
        registry.sync(1, false, &awake);
        assert_eq!(awake.status().agents, 0);
        assert!(!awake.status().asserted);
        assert_eq!(backend.creates(), 1);
    }

    #[test]
    fn each_view_counts_as_one_agent() {
        let (awake, _backend) = fixture(true);
        let mut registry = Registry::new();
        registry.sync(1, true, &awake);
        registry.sync(2, true, &awake);
        assert_eq!(awake.status().agents, 2);
    }

    /// A tab closed mid-turn: the release hook empties the entry and lets go.
    #[test]
    fn releasing_a_view_drops_its_hold_and_its_entry() {
        let (awake, _backend) = fixture(true);
        let mut registry = Registry::new();
        registry.sync(1, true, &awake);
        registry.release(&1);
        assert!(registry.slots.is_empty());
        assert_eq!(awake.status().agents, 0);
        assert!(!awake.status().asserted);
    }

    /// The wiring, not just the table: a released view lets go of its hold
    /// and leaves no entry behind.
    #[gpui::test]
    fn releasing_the_view_releases_its_hold(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        struct View;
        let (awake, _backend) = fixture(true);
        let view = cx.new(|_| View);
        let key = (std::thread::current().id(), view.entity_id());
        view.update(cx, |_, cx| sync_to(&awake, true, cx));
        assert_eq!(awake.status().agents, 1);
        view.update(cx, |_, cx| sync_to(&awake, false, cx));
        assert_eq!(awake.status().agents, 0, "turn over");
        view.update(cx, |_, cx| sync_to(&awake, true, cx));
        drop(view);
        // Dropped entities are released when the app next flushes effects.
        cx.update(|_| {});
        assert_eq!(awake.status().agents, 0, "a view closed mid-turn lets go");
        assert!(!holds().slots.contains_key(&key), "and leaves no entry");
    }

    #[test]
    fn off_mode_counts_the_turn_without_asserting() {
        let (awake, _backend) = fixture(true);
        awake.set_mode(crate::agent_awake::AwakeMode::Off);
        let mut registry = Registry::new();
        registry.sync(1, true, &awake);
        assert_eq!(awake.status().agents, 1);
        assert!(!awake.status().asserted);
    }
}
