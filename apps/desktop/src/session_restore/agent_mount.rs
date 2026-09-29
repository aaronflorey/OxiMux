//! Putting a started agent session on screen: the shared tail of a cockpit
//! agent tab's restore.
//!
//! Boot restore and the in-place resume after a daemon restart both end the
//! same way once they hold a started session — claim its `agent_sessions` row,
//! mount it (a new tab, or in place of the lost session), and, on a cold
//! resume, watch for the CLI refusing the persisted conversation and fall back
//! to a fresh one exactly once. Only where the session is mounted differs,
//! which [`AgentMount`] names.

use std::sync::Arc;

use gpui::{AsyncWindowContext, WeakEntity};
use oximux_agents::{AgentRuntime, AgentSessionConfig, AgentStatusStream, CliRuntime, SharedBackend};
use oximux_core::AgentSessionId;
use oximux_pty::TerminalSessionId;

use crate::persisted_terminals::PersistedAgentTab;
use crate::relay_cold_restore::RestoreMarker;
use crate::shell::pane_group::RestoredTabMeta;
use crate::shell::pane_tree::PaneGroupId;
use crate::shell::project_panes::ProjectPanes;
use crate::workspace_root::WorkspaceRoot;

/// Where the started session goes.
pub(crate) enum AgentMount {
    /// Boot restore: a new tab in `target_group` (`None`: the active group,
    /// for a legacy single-group restore), settled into its saved slot.
    Push {
        target_group: Option<PaneGroupId>,
        label: String,
        meta: RestoredTabMeta,
    },
}

/// A started agent session and what finishing its restore needs.
pub(crate) struct StartedAgent {
    pub cli_runtime: Arc<CliRuntime>,
    /// The tab's persisted form: its row fields, and the tab a push mounts.
    pub persisted: PersistedAgentTab,
    pub adapter_id: &'static str,
    pub session_id: AgentSessionId,
    pub backend: SharedBackend,
    pub term_id: TerminalSessionId,
    pub status_rx: AgentStatusStream,
    /// Adopted the still-running process rather than starting one.
    pub warm: bool,
    /// Started with the persisted conversation to resume.
    pub attempted_resume: bool,
    /// The fresh-start config for the rejected-resume fallback.
    pub fresh_cfg: AgentSessionConfig,
    /// The dead PTY whose ambient reading is dropped on a cold start.
    pub dead_pty: Option<String>,
    pub mount: AgentMount,
}

/// Claim, mount and — on a cold resume — watch `started` through to its final
/// session. Runs on the workspace root's window.
pub(crate) async fn finish_agent_restore(
    root: WeakEntity<WorkspaceRoot>,
    panes: WeakEntity<ProjectPanes>,
    started: StartedAgent,
    cx: &mut AsyncWindowContext,
) {
    let StartedAgent {
        cli_runtime,
        persisted,
        adapter_id,
        session_id,
        backend,
        term_id,
        status_rx,
        warm,
        attempted_resume,
        fresh_cfg,
        dead_pty,
        mount,
    } = started;
    // Re-adopt the pre-restart agent_sessions row (the boot sweep marked it
    // Interrupted): `restoring = true` flips that same row back to live and
    // keeps its persisted title, instead of orphaning it and inserting a
    // duplicate "Claude Code" row. On a cold RESUME the claim waits for the
    // watchdog's verdict (at most `ROW_CLAIM_GRACE`), so a quickly refused
    // resume lets the fresh session claim the row directly.
    let cold_resume = !warm && attempted_resume;
    // On a cold resume every reader — the tab, its watcher, the rail row —
    // reads a proxy of the session's stream rather than the stream itself.
    // The watchdog forwards into it and holds back only a refused resume's
    // failed exit, so nothing ever shows the refusal as a failure, and the
    // fallback carries on the same proxy with the fresh session.
    let (proxy_tx, proxy_rx) = tokio::sync::watch::channel(status_rx.borrow().clone());
    let tab_rx = if cold_resume { proxy_rx.clone() } else { status_rx.clone() };
    let claim_row = |root: &WeakEntity<WorkspaceRoot>,
                     cx: &mut AsyncWindowContext,
                     session_id: AgentSessionId,
                     status_rx: AgentStatusStream| {
        let _ = root.update(cx, |this, cx| {
            crate::shell::agent_session_persistence::spawn_for_session(
                this,
                persisted.worktree_path.clone(),
                adapter_id,
                persisted.model.clone(),
                persisted.effort.clone(),
                session_id,
                status_rx,
                true,
                cx,
            );
        });
    };
    if !cold_resume {
        claim_row(&root, cx, session_id, status_rx.clone());
    }
    // A warm re-attach prints nothing (the live PTY holds the
    // conversation); a cold spawn says which of the two things happened.
    let mount_marker = (!warm).then_some(if attempted_resume {
        RestoreMarker::Resumed
    } else {
        RestoreMarker::StartedFresh
    });
    // Fetched before the mount so no early return below can leave a
    // mounted tab reading a proxy nobody feeds.
    let Ok(executor) = cx.update(|_, cx| cx.background_executor().clone()) else {
        let _ = cli_runtime.cancel(session_id).await;
        return;
    };
    let target_group = match &mount {
        AgentMount::Push { target_group, .. } => *target_group,
    };
    let mounted = match mount {
        AgentMount::Push { target_group, label, meta } => panes
            .update_in(cx, |p, window, cx| match target_group {
                Some(group_id) => p.push_restored_agent_tab_in(
                    group_id,
                    &persisted,
                    adapter_id,
                    label,
                    session_id,
                    tab_rx.clone(),
                    backend,
                    term_id,
                    meta,
                    mount_marker,
                    window,
                    cx,
                ),
                None => p.push_restored_agent_tab(
                    &persisted,
                    adapter_id,
                    label,
                    session_id,
                    tab_rx.clone(),
                    backend,
                    term_id,
                    meta,
                    mount_marker,
                    window,
                    cx,
                ),
            })
            .is_ok(),
    };
    if !mounted {
        tracing::warn!(
            ?session_id,
            "agent restore: workspace dropped mid-spawn; cancelling orphan"
        );
        let _ = cli_runtime.cancel(session_id).await;
        return;
    }
    // The dead PTY's ambient reading can never be re-seeded (a cockpit tab
    // never cold-restores through the plain-terminal offer path), so drop
    // it rather than leave it for the 7-day retention to collect.
    if !warm && let Some(dead_pty) = dead_pty {
        let _ = cx.update(|_, cx| {
            let ticket = crate::shell::ambient_state::ticket();
            cx.background_executor()
                .spawn(async move { crate::shell::ambient_state::forget(&dead_pty, ticket) })
                .detach();
        });
    }
    if !cold_resume {
        tracing::info!(adapter = adapter_id, warm, resumed = false, fallback = false, "agent restore");
        return;
    }
    // Resume watchdog: a CLI handed an id it has no transcript for exits
    // non-zero before it reports anything — within a second or two, or
    // only once the user dismisses a startup menu the CLI painted first.
    // Respawn fresh exactly once under the honest marker; silence never
    // triggers (see `agent_resume::should_fallback`).
    use crate::session_restore::agent_resume::{
        ROW_CLAIM_GRACE, RESUME_VERDICT_CEILING, ResumeVerdict, forward, verdict_within,
    };
    let mut inner = status_rx;
    let mut claimed = false;
    let mut verdict =
        verdict_within(&mut inner, &proxy_tx, &executor, ROW_CLAIM_GRACE).await;
    if verdict.is_none() {
        claim_row(&root, cx, session_id, proxy_rx.clone());
        claimed = true;
        verdict = verdict_within(
            &mut inner,
            &proxy_tx,
            &executor,
            RESUME_VERDICT_CEILING - ROW_CLAIM_GRACE,
        )
        .await;
    }
    let rejected = matches!(verdict, Some(ResumeVerdict::Rejected(_)));
    let session_id = if let Some(ResumeVerdict::Rejected(refusal)) = verdict {
        tracing::warn!(adapter = adapter_id, "agent restore: CLI rejected the persisted session; starting fresh");
        let _ = cli_runtime.cancel(session_id).await;
        let Some(fresh) = start_fresh_agent_session(&cli_runtime, fresh_cfg, adapter_id).await else {
            // No fresh session to show instead: publish the refusal after
            // all, so the tab and row read failed rather than frozen.
            proxy_tx.send_replace(refusal);
            return;
        };
        let (fresh_id, fresh_backend, fresh_term, fresh_rx) = fresh;
        let swapped = panes.update(cx, |p, cx| {
            p.replace_restored_agent_session(
                target_group,
                session_id,
                fresh_id,
                proxy_rx.clone(),
                fresh_backend,
                fresh_term,
                RestoreMarker::StartedFresh,
                cx,
            )
        });
        if !matches!(swapped, Ok(true)) {
            tracing::warn!(?fresh_id, "agent restore: tab gone before fallback; cancelling orphan");
            let _ = cli_runtime.cancel(fresh_id).await;
            return;
        }
        // A late refusal (behind a startup menu) lands after the row was
        // claimed; point it at the fresh session so a rail click still
        // finds the tab (parked if the claim has not registered yet).
        if claimed {
            let _ = root.update(cx, |this, cx| this.repoint_live_agent(session_id, fresh_id, cx));
        }
        inner = fresh_rx;
        fresh_id
    } else {
        session_id
    };
    if !claimed {
        claim_row(&root, cx, session_id, proxy_rx);
    }
    tracing::info!(adapter = adapter_id, warm, resumed = !rejected, fallback = rejected, "agent restore");
    forward(inner, &proxy_tx).await;
}

/// Spawn a fresh (non-resumed) agent session and subscribe to it: the resume
/// fallback's spawn, mirroring the cold-spawn arm of a restore.
/// `None` when any step fails (logged); a session that was started is
/// cancelled before returning so nothing is orphaned.
async fn start_fresh_agent_session(
    cli_runtime: &Arc<CliRuntime>,
    cfg: AgentSessionConfig,
    adapter_id: &'static str,
) -> Option<(AgentSessionId, SharedBackend, TerminalSessionId, AgentStatusStream)> {
    let session_id = match cli_runtime.start_session(cfg).await {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, adapter = adapter_id, "agent restore: fallback start_session failed");
            return None;
        }
    };
    let wired = cli_runtime.backend_for(session_id).and_then(|backend| {
        let term_id = cli_runtime.terminal_session_id(session_id)?;
        let status_rx = cli_runtime.subscribe_status(session_id)?;
        Ok((session_id, backend, term_id, status_rx))
    });
    match wired {
        Ok(t) => Some(t),
        Err(err) => {
            tracing::warn!(?err, adapter = adapter_id, "agent restore: fallback wiring failed");
            let _ = cli_runtime.cancel(session_id).await;
            None
        }
    }
}
