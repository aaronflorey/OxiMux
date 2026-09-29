//! Putting an agent session on screen: the shared tail of a cockpit agent
//! tab's restore, and the in-place resume after a daemon restart.
//!
//! Boot restore and the in-place resume after a daemon restart both end the
//! same way once they hold a started session — claim its `agent_sessions` row,
//! mount it (a new tab, or in place of the lost session), and, on a cold
//! resume, watch for the CLI refusing the persisted conversation and fall back
//! to a fresh one exactly once. Only where the session is mounted differs,
//! which [`AgentMount`] names.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AsyncWindowContext, Context, WeakEntity, Window};
use oximux_agents::{AgentRuntime, AgentSessionConfig, AgentStatusStream, CliRuntime, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;

use crate::persisted_terminals::PersistedAgentTab;
use crate::relay_cold_restore::RestoreMarker;
use crate::shell::pane_group::{PaneGroup, RestoredTabMeta};
use crate::shell::pane_tree::PaneGroupId;
use crate::shell::project_panes::ProjectPanes;
use crate::workspace_root::WorkspaceRoot;

const DEFAULT_AGENT_COLS: u16 = 120;
const DEFAULT_AGENT_ROWS: u16 = 32;

pub(crate) fn static_adapter_id(adapter: AgentAdapter) -> &'static str {
    match adapter {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Pi => "pi",
        AgentAdapter::Omp => "omp",
        AgentAdapter::Custom => "custom",
    }
}

/// How to start a persisted agent tab's CLI again.
pub(crate) struct LaunchConfig {
    pub adapter_id: &'static str,
    /// Spawns the CLI on its own conversation when one was captured and the
    /// adapter can resume it.
    pub cfg: AgentSessionConfig,
    pub attempted_resume: bool,
    /// The conversation id, for seeding a warm re-attach's status.
    pub known_session: Option<String>,
}

pub(crate) fn launch_config(persisted: &PersistedAgentTab, cx: &App) -> LaunchConfig {
    let adapter_id: &'static str = static_adapter_id(persisted.adapter);
    // On a respawn (PTY no longer alive in the daemon) re-apply the current
    // per-agent launch flags so a restored agent comes back with the same
    // defaults a fresh launch would use. Ignored on warm re-attach, which
    // adopts the already-running process and never reads cfg.
    // The profile the tab was launched under, so a respawn reaches the same
    // endpoint/account rather than silently falling back to `default`.
    let profile = persisted.profile.clone();
    let (extra_args, env) = cx
        .try_global::<oximux_settings::AgentLaunchSettings>()
        .map(|d| {
            (
                d.args_for_in(adapter_id, profile.as_deref()),
                d.env_for(adapter_id, profile.as_deref()),
            )
        })
        .unwrap_or_default();
    // Cold spawn resumes the agent's OWN conversation when the snapshot
    // captured its id and the adapter can (`claude --resume`, `codex resume`,
    // …). The cold path spawns with it; a warm re-attach adopts the live
    // process, whose conversation never went anywhere, and only seeds its
    // status with the id so an idle agent keeps naming it.
    let resumption = crate::session_restore::agent_resume::restore_resumption(
        persisted.adapter,
        persisted.provider_session.as_deref(),
    );
    let attempted_resume = !matches!(resumption, oximux_core::SessionResumption::None);
    let known_session = resumption.source_id().map(str::to_owned);
    let cfg = AgentSessionConfig {
        adapter: persisted.adapter,
        worktree_path: PathBuf::from(&persisted.worktree_path),
        prompt: None,
        model: persisted.model.clone(),
        effort: persisted.effort.clone(),
        extra_args,
        env,
        cols: DEFAULT_AGENT_COLS,
        rows: DEFAULT_AGENT_ROWS,
        custom_command: None,
        resumption,
    };
    LaunchConfig { adapter_id, cfg, attempted_resume, known_session }
}

/// Where the started session goes.
pub(crate) enum AgentMount {
    /// Boot restore: a new tab in `target_group` (`None`: the active group,
    /// for a legacy single-group restore), settled into its saved slot.
    Push {
        panes: WeakEntity<ProjectPanes>,
        target_group: Option<PaneGroupId>,
        label: String,
        meta: RestoredTabMeta,
    },
    /// After a daemon restart: in place of `old`, the lost session behind an
    /// existing tab of `group`, which keeps its slot, label and colour — in
    /// the pane showing `terminal` (the agent's own, even in a split tab).
    Replace { group: WeakEntity<PaneGroup>, old: AgentSessionId, terminal: TerminalSessionId },
}

/// Where a rejected resume's fresh session is swapped in: the same tab.
enum SwapTarget {
    Panes { panes: WeakEntity<ProjectPanes>, target_group: Option<PaneGroupId> },
    /// The pane now showing the resumed session's terminal.
    Group { group: WeakEntity<PaneGroup>, terminal: TerminalSessionId },
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
    let swap_target = match &mount {
        AgentMount::Push { panes, target_group, .. } => {
            SwapTarget::Panes { panes: panes.clone(), target_group: *target_group }
        }
        AgentMount::Replace { group, .. } => SwapTarget::Group { group: group.clone(), terminal: term_id },
    };
    let mounted = match mount {
        AgentMount::Push { panes, target_group, label, meta } => panes
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
        // Never warm (the daemon that held the process is gone), so there
        // is always a marker.
        AgentMount::Replace { group, old, terminal } => matches!(
            group.update(cx, |g, cx| g.replace_agent_session(
                old,
                session_id,
                tab_rx.clone(),
                backend,
                term_id,
                mount_marker.unwrap_or(RestoreMarker::StartedFresh),
                Some(terminal),
                cx,
            )),
            Ok(true)
        ),
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
        let Some(fresh) = start_agent_session(&cli_runtime, fresh_cfg, adapter_id).await else {
            // No fresh session to show instead: publish the refusal after
            // all, so the tab and row read failed rather than frozen.
            proxy_tx.send_replace(refusal);
            return;
        };
        let (fresh_id, fresh_backend, fresh_term, fresh_rx) = fresh;
        let swapped = match &swap_target {
            SwapTarget::Panes { panes, target_group } => panes.update(cx, |p, cx| {
                p.replace_restored_agent_session(
                    *target_group,
                    session_id,
                    fresh_id,
                    proxy_rx.clone(),
                    fresh_backend,
                    fresh_term,
                    RestoreMarker::StartedFresh,
                    cx,
                )
            }),
            SwapTarget::Group { group, terminal } => group.update(cx, |g, cx| {
                g.replace_agent_session(
                    session_id,
                    fresh_id,
                    proxy_rx.clone(),
                    fresh_backend,
                    fresh_term,
                    RestoreMarker::StartedFresh,
                    Some(*terminal),
                    cx,
                )
            }),
        };
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

/// Start an agent session and subscribe to it: the resume fallback's fresh
/// spawn, and the in-place resume's. `None` when any step fails (logged); a
/// session that was started is cancelled before returning so nothing is
/// orphaned.
async fn start_agent_session(
    cli_runtime: &Arc<CliRuntime>,
    cfg: AgentSessionConfig,
    adapter_id: &'static str,
) -> Option<(AgentSessionId, SharedBackend, TerminalSessionId, AgentStatusStream)> {
    let session_id = match cli_runtime.start_session(cfg).await {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, adapter = adapter_id, "agent restore: start_session failed");
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
            tracing::warn!(?err, adapter = adapter_id, "agent restore: session wiring failed");
            let _ = cli_runtime.cancel(session_id).await;
            None
        }
    }
}

/// A cockpit agent tab whose session died with its daemon.
pub(crate) struct LostAgent {
    /// The tab as it would be persisted now — its conversation id included.
    pub persisted: PersistedAgentTab,
    pub old_session: AgentSessionId,
    /// The agent's own pane's (lost) terminal session.
    pub terminal: TerminalSessionId,
    /// The lost PTY, whose ambient reading is dropped.
    pub dead_pty: Option<String>,
    pub group: WeakEntity<PaneGroup>,
}

/// Bring a lost agent tab back in place: end the old runtime session, start
/// the CLI again on its own conversation, and swap it into the same tab — with
/// boot restore's row claim, marker and rejected-resume fallback. Uses this
/// window's runtime, which owns the tab's session.
pub(crate) fn resume_agent_in_place(
    root: &mut WorkspaceRoot,
    lost: LostAgent,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) {
    if matches!(lost.persisted.adapter, AgentAdapter::Custom) {
        tracing::info!("agent resume: Custom adapter is not restartable; tab stays exited");
        return;
    }
    let cli_runtime = Arc::clone(&root.cli_runtime);
    let LaunchConfig { adapter_id, cfg, attempted_resume, .. } = launch_config(&lost.persisted, cx);
    let fresh_cfg = AgentSessionConfig {
        resumption: oximux_core::SessionResumption::None,
        ..cfg.clone()
    };
    cx.spawn_in(window, async move |root, cx| {
        // The old session's process died with its daemon; retire its runtime
        // entry before a new one takes the tab.
        let _ = cli_runtime.cancel(lost.old_session).await;
        let Some((session_id, backend, term_id, status_rx)) =
            start_agent_session(&cli_runtime, cfg, adapter_id).await
        else {
            // The tab keeps its "process exited" banner.
            return;
        };
        tracing::info!(adapter = adapter_id, "agent resume: restarting a tab lost with its daemon");
        let started = StartedAgent {
            cli_runtime,
            persisted: lost.persisted,
            adapter_id,
            session_id,
            backend,
            term_id,
            status_rx,
            warm: false,
            attempted_resume,
            fresh_cfg,
            dead_pty: lost.dead_pty,
            mount: AgentMount::Replace {
                group: lost.group,
                old: lost.old_session,
                terminal: lost.terminal,
            },
        };
        finish_agent_restore(root, started, cx).await;
    })
    .detach();
}
