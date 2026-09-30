//! Persistence for the keep-awake [`AwakeMode`]: one flat `awake.mode` key,
//! plus the pre-mode `notify.agent_awake` bool kept in step with it.
//!
//! The legacy key is still written so an older build reading the same profile
//! sees the matching on/off choice, and it is still read so a choice an older
//! build made *after* this one is not overridden by a stale `awake.mode`: when
//! the two disagree, only an older build can have written the bool last,
//! because [`apply`] — the one writer here — always writes both.

use oximux_storage::SettingsRepo;

use crate::agent_awake::{self, AwakeMode};

pub const MODE_KEY: &str = "awake.mode";
/// The pre-mode "Keep this computer awake while agents run" toggle.
pub const LEGACY_AGENT_KEY: &str = "notify.agent_awake";

pub fn parse(s: &str) -> Option<AwakeMode> {
    match s {
        "on" => Some(AwakeMode::On),
        "agent" => Some(AwakeMode::Agent),
        "off" => Some(AwakeMode::Off),
        _ => None,
    }
}

/// The user-facing name, shared by the Settings row and the status chip.
pub fn label(mode: AwakeMode) -> &'static str {
    match mode {
        AwakeMode::On => "On",
        AwakeMode::Agent => "Agent",
        AwakeMode::Off => "Off",
    }
}

pub fn as_str(mode: AwakeMode) -> &'static str {
    match mode {
        AwakeMode::On => "on",
        AwakeMode::Agent => "agent",
        AwakeMode::Off => "off",
    }
}

/// The persisted mode. An unknown or missing value falls back rather than
/// failing; see the module doc for why a disagreeing legacy bool wins.
pub fn load(get: impl Fn(&str) -> Option<String>) -> AwakeMode {
    let mode = get(MODE_KEY).as_deref().and_then(parse);
    let legacy = match get(LEGACY_AGENT_KEY).as_deref() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    let from_legacy = |on: bool| if on { AwakeMode::Agent } else { AwakeMode::Off };
    match (mode, legacy) {
        (None, legacy) => from_legacy(legacy.unwrap_or(true)),
        (Some(mode), Some(legacy)) if legacy != (mode != AwakeMode::Off) => from_legacy(legacy),
        (Some(mode), _) => mode,
    }
}

/// Apply `mode` to the live assertion and persist it. The single entry point
/// for the Settings row and the status-bar popover.
pub fn apply(repo: &SettingsRepo, mode: AwakeMode) {
    agent_awake::global().set_mode(mode);
    persist(repo, mode);
}

/// Write both keys, always together — [`load`]'s disagreement rule depends on
/// this build never writing only one of them. The legacy key goes first and
/// a failure stops the second write, so the pair never disagrees because of
/// us: if the second write fails, the legacy key already holds the new choice
/// and wins on load; if the first fails, both keep the previous choice. The
/// live mode is applied either way — only its survival across a restart is
/// at stake.
fn persist(repo: &SettingsRepo, mode: AwakeMode) {
    let legacy = if mode == AwakeMode::Off { "false" } else { "true" };
    for (key, value) in [(LEGACY_AGENT_KEY, legacy), (MODE_KEY, as_str(mode))] {
        if let Err(err) = repo.set(key, value) {
            tracing::warn!(key, %err, "failed to persist keep-awake mode");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load_from(pairs: &[(&str, &str)]) -> AwakeMode {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        load(|k| map.get(k).cloned())
    }

    #[test]
    fn parse_and_as_str_round_trip() {
        for mode in [AwakeMode::On, AwakeMode::Agent, AwakeMode::Off] {
            assert_eq!(parse(as_str(mode)), Some(mode));
        }
        assert_eq!(parse("sometimes"), None);
    }

    #[test]
    fn a_stored_mode_is_used() {
        assert_eq!(load_from(&[(MODE_KEY, "on")]), AwakeMode::On);
        assert_eq!(load_from(&[(MODE_KEY, "off")]), AwakeMode::Off);
    }

    #[test]
    fn without_a_mode_the_legacy_toggle_decides() {
        assert_eq!(load_from(&[(LEGACY_AGENT_KEY, "false")]), AwakeMode::Off);
        assert_eq!(load_from(&[(LEGACY_AGENT_KEY, "true")]), AwakeMode::Agent);
        assert_eq!(load_from(&[]), AwakeMode::Agent, "a fresh profile");
        assert_eq!(
            load_from(&[(MODE_KEY, "sometimes"), (LEGACY_AGENT_KEY, "false")]),
            AwakeMode::Off,
            "garbage mode falls back to the legacy toggle"
        );
    }

    /// Downgrade → toggle in the old build → upgrade: the legacy bool was
    /// written last, so it wins over the stale mode.
    #[test]
    fn a_disagreeing_legacy_toggle_wins() {
        assert_eq!(
            load_from(&[(MODE_KEY, "agent"), (LEGACY_AGENT_KEY, "false")]),
            AwakeMode::Off
        );
        assert_eq!(
            load_from(&[(MODE_KEY, "off"), (LEGACY_AGENT_KEY, "true")]),
            AwakeMode::Agent
        );
        assert_eq!(
            load_from(&[(MODE_KEY, "on"), (LEGACY_AGENT_KEY, "false")]),
            AwakeMode::Off
        );
    }

    #[test]
    fn an_agreeing_legacy_toggle_keeps_the_mode() {
        assert_eq!(
            load_from(&[(MODE_KEY, "on"), (LEGACY_AGENT_KEY, "true")]),
            AwakeMode::On
        );
        assert_eq!(
            load_from(&[(MODE_KEY, "off"), (LEGACY_AGENT_KEY, "false")]),
            AwakeMode::Off
        );
    }

    /// Pins the invariant the disagreement rule relies on: every write keeps
    /// the two keys agreeing, so a reload gives back exactly what was written.
    #[test]
    fn persist_writes_both_keys_in_agreement() {
        let repo = SettingsRepo::new(oximux_storage::open_memory().unwrap());
        for mode in [AwakeMode::On, AwakeMode::Off, AwakeMode::Agent] {
            persist(&repo, mode);
            assert_eq!(repo.get(MODE_KEY).unwrap().as_deref(), Some(as_str(mode)));
            let legacy = repo.get(LEGACY_AGENT_KEY).unwrap();
            assert_eq!(legacy.as_deref(), Some(if mode == AwakeMode::Off { "false" } else { "true" }));
            assert_eq!(load(|k| repo.get(k).ok().flatten()), mode);
        }
    }
}
