//! Bringing a shell back after its daemon was replaced.
//!
//! The view stays mounted through the loss (its tab, split and title are the
//! user's layout); only the session behind it changes. The replacement is the
//! production cold-restore path — a relay spawn prefilled with the lost
//! session's final checkpoint — with a "terminal daemon restarted" marker
//! under the history instead of "session restored".

use std::sync::Arc;

use gpui::{AsyncApp, Context, WeakEntity};

use super::{TerminalView, spawn_relay_pty_sized};
use crate::relay_cold_restore::{self, RestoreMarker, marker};

impl TerminalView {
    /// Whether this view's session died with its daemon and has not been
    /// brought back yet.
    pub fn is_lost_to_daemon(&self) -> bool {
        self.lost_to_daemon
    }

    /// Whether this view's session lives on `backend` (the same shared handle).
    pub(crate) fn shares_backend(&self, backend: &super::SharedBackend) -> bool {
        std::sync::Arc::ptr_eq(&self.backend, backend)
    }

    /// Whether a replacement for a lost session is being spawned.
    pub fn is_recovering_from_loss(&self) -> bool {
        self.recovering_from_loss
    }

    /// The daemon-side id of this view's session, as captured when it was
    /// mounted — still known after the daemon that minted it is gone.
    pub(crate) fn relay_pty_id(&self) -> Option<String> {
        self.relay_pty_id.clone()
    }

    /// Replace a session lost with its daemon by a fresh shell on the new
    /// daemon, prefilled with the lost session's scrollback. A no-op unless
    /// the view is lost, and once already under way. The spawn and the disk
    /// reads run off the UI thread.
    ///
    /// Relay only: if the new daemon will not take the session the view keeps
    /// its "process exited" banner rather than coming back in-process, where
    /// it would look recovered yet die with the app.
    pub fn respawn_after_loss(&mut self, cx: &mut Context<Self>) {
        if !self.lost_to_daemon || self.recovering_from_loss {
            return;
        }
        self.recovering_from_loss = true;
        let lost_pty = self.relay_pty_id.clone();
        let env = self.ids.env();
        let grid = (self.last_resize.0 > 0 && self.last_resize.1 > 0).then_some(self.last_resize);
        let checkpoints = relay_cold_restore::default_checkpoints_dir();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let spawned = cx
                .background_executor()
                .spawn(async move { spawn_replacement(checkpoints, lost_pty, env, grid) })
                .await;
            // Held outside the view update so a replacement that cannot be
            // delivered — the pane was closed, or came back some other way,
            // while the spawn ran — is still closed rather than left running
            // in the daemon, which outlives the app.
            let mut pending = spawned;
            let _ = this.update(cx, |view, cx| {
                view.recovering_from_loss = false;
                if pending.is_none() {
                    tracing::warn!("could not bring a terminal back on the new daemon");
                    return;
                }
                if !view.lost_to_daemon {
                    return;
                }
                let Some(replacement) = pending.take() else {
                    return;
                };
                view.replace_live_session(Arc::clone(&replacement.backend), replacement.session_id, cx);
                cx.emit(super::TerminalViewEvent::Recovered { session_id: replacement.session_id });
                if replacement.external_id.is_none() {
                    tracing::debug!("recovered terminal has no daemon id");
                }
                if let Some(line) = replacement.resume_line {
                    view.queue_input_on_first_output(line.into_bytes(), cx);
                }
                // Consumed only once delivered: undelivered, a quit keeps the
                // lost session restorable on the next launch.
                if let Some(pty) = replacement.lost_pty {
                    let checkpoints = replacement.consume_checkpoint.then_some(replacement.checkpoints).flatten();
                    let ticket = crate::shell::ambient_state::ticket();
                    cx.background_executor()
                        .spawn(async move {
                            if let Some(dir) = checkpoints {
                                relay_cold_restore::consume_checkpoint(&dir, &pty);
                            }
                            crate::shell::ambient_state::forget(&pty, ticket);
                        })
                        .detach();
                }
            });
            if let Some(orphan) = pending {
                close_orphan(orphan, cx);
            }
        })
        .detach();
    }
}

struct Replacement {
    backend: super::SharedBackend,
    session_id: oximux_pty::TerminalSessionId,
    external_id: Option<String>,
    /// A hand-typed agent's resume command, pre-typed at the new prompt.
    resume_line: Option<String>,
    checkpoints: Option<std::path::PathBuf>,
    /// The lost session, whose ambient record (and checkpoint, when
    /// `consume_checkpoint`) the delivered replacement used up.
    lost_pty: Option<String>,
    consume_checkpoint: bool,
}

/// Close a replacement nobody took, off the UI thread (a daemon round-trip).
/// Skipped while quitting: views are being torn down on purpose, and the
/// daemon keeps sessions across a quit.
fn close_orphan(orphan: Replacement, cx: &mut AsyncApp) {
    if super::APP_QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    cx.background_executor()
        .spawn(async move {
            let mut backend = orphan.backend.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(err) = backend.close(orphan.session_id) {
                tracing::debug!(?err, "closing an undelivered terminal replacement failed");
            }
        })
        .detach();
}

/// What a recovered shell's grid holds before its first output: the lost
/// session's history closed by one "terminal daemon restarted" marker (just
/// the marker when there is no history), then the resume hint when an agent's
/// resume command will be pre-typed.
fn recovery_prefill(history: Option<&[u8]>, resume_label: Option<&str>) -> Vec<u8> {
    let mut prefill = match history {
        // `read_cold_restore` already closed it with the marker.
        Some(bytes) if !bytes.is_empty() => bytes.to_vec(),
        _ => marker(RestoreMarker::DaemonRestarted).to_vec(),
    };
    if let Some(label) = resume_label {
        prefill.extend(relay_cold_restore::resume_hint(label));
    }
    prefill
}

/// Blocking: disk reads and a daemon round-trip.
fn spawn_replacement(
    checkpoints: Option<std::path::PathBuf>,
    lost_pty: Option<String>,
    env: Vec<(String, String)>,
    grid: Option<(u16, u16)>,
) -> Option<Replacement> {
    let cold = match (&checkpoints, lost_pty.as_deref()) {
        (Some(dir), Some(id)) => {
            relay_cold_restore::read_cold_restore(dir, id, RestoreMarker::DaemonRestarted)
        }
        _ => None,
    };
    // A hand-typed agent in the lost shell left its label and conversation id
    // in the ambient record, as on a cold boot: the fresh shell gets the
    // resume command pre-typed (never sent) under a dim hint.
    let resume = lost_pty
        .as_deref()
        .and_then(crate::shell::ambient_state::load_for_resume)
        .and_then(|offer| {
            crate::session_restore::agent_resume::resume_shell_line(
                &offer.agent_label,
                &offer.session_id,
            )
            .map(|line| (offer.agent_label, line))
        });
    let prefill = recovery_prefill(
        cold.as_ref().map(|restore| restore.bytes.as_slice()),
        resume.as_ref().map(|(label, _)| label.as_str()),
    );
    // Where the lost shell really was (the daemon's last kernel-read cwd),
    // else home.
    let cwd = cold
        .as_ref()
        .and_then(|restore| restore.cwd.clone())
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let dims = grid.or_else(|| cold.as_ref().and_then(|restore| restore.dims));
    let (backend, session_id) = spawn_relay_pty_sized(cwd, env, dims, &prefill)?;
    let external_id = backend
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .external_id_of(session_id);
    Some(Replacement {
        backend,
        session_id,
        external_id,
        consume_checkpoint: cold.is_some(),
        lost_pty: if cold.is_some() || resume.is_some() { lost_pty } else { None },
        resume_line: resume.map(|(_, line)| line),
        checkpoints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_count(bytes: &[u8]) -> usize {
        String::from_utf8_lossy(bytes).matches("--- terminal daemon restarted ---").count()
    }

    #[test]
    fn a_shell_without_history_comes_back_with_just_the_marker() {
        let prefill = recovery_prefill(None, None);
        assert_eq!(prefill, marker(RestoreMarker::DaemonRestarted));
        assert_eq!(recovery_prefill(Some(b""), None), prefill, "cwd-only checkpoint");
    }

    #[test]
    fn history_keeps_its_one_marker_and_the_hint_goes_below() {
        let dir = tempfile::tempdir().unwrap();
        let pty = dir.path().join("pty-1");
        std::fs::create_dir_all(&pty).unwrap();
        std::fs::write(
            pty.join("meta.json"),
            br#"{"cwd":"","cols":80,"rows":24,"started_at_epoch_secs":0,"ended_at_epoch_secs":null}"#,
        )
        .unwrap();
        std::fs::write(pty.join("scrollback.bin"), b"$ make test\r\nok\r\n").unwrap();
        let restore =
            relay_cold_restore::read_cold_restore(dir.path(), "pty-1", RestoreMarker::DaemonRestarted)
                .expect("restorable");

        let prefill = recovery_prefill(Some(&restore.bytes), Some("Claude Code"));

        let text = String::from_utf8_lossy(&prefill);
        assert!(text.contains("make test"), "the lost history is back");
        assert_eq!(marker_count(&prefill), 1, "one marker, not two");
        assert!(!text.contains("session restored"));
        let marker_at = text.find("terminal daemon restarted").unwrap();
        let hint_at = text.find("Claude Code").expect("hint present");
        assert!(hint_at > marker_at, "the hint sits under the marker");
    }
}
