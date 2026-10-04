//! A remote terminal's local emulator. Transport work belongs to the caller;
//! this backend never spawns a process or interprets server paths locally.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use anyhow::{Result, bail};
use crate::{Cell, InputMode, MouseMode, OutputWaker, SpawnConfig, TerminalBackend,
    TerminalEvent, TerminalSessionId, TerminalSnapshot, TerminalState};

pub const REMOTE_SESSION: TerminalSessionId = TerminalSessionId(1);

#[derive(Debug, PartialEq)]
pub enum RemoteTerminalCommand {
    Input(Vec<u8>),
    Resize(u16, u16),
    Detach,
}

pub type RemoteTerminalSender = Arc<dyn Fn(RemoteTerminalCommand) -> Result<()> + Send + Sync>;

struct State {
    grid: TerminalState,
    binding: u64,
    events: VecDeque<TerminalEvent>,
    sender: Option<RemoteTerminalSender>,
    writable: bool,
    ready: bool,
    closed: bool,
    exited: bool,
    waker: Option<OutputWaker>,
}

impl State {
    fn emit(&mut self, event: TerminalEvent) {
        // The grid already contains the output. Bound notification retention;
        // a surviving Output/Resize event asks the renderer for the whole grid.
        if self.events.len() == 512 { self.events.pop_front(); }
        self.events.push_back(event);
        if let Some(waker) = &self.waker { waker(); }
    }

    fn send(&self, command: RemoteTerminalCommand) -> Result<()> {
        if self.closed || self.exited || !self.ready || !self.writable {
            bail!("Remote terminal input is unavailable or read-only");
        }
        let Some(sender) = &self.sender else { bail!("Remote terminal is disconnected"); };
        // Queue acceptance only. The transport owner surfaces actual RPC failures.
        sender(command)
    }
}

#[derive(Clone)]
pub struct RemoteTerminalControl(Arc<Mutex<State>>);

impl RemoteTerminalControl {
    pub fn bind(&self, sender: Option<RemoteTerminalSender>, writable: bool) -> RemoteTerminalFeed {
        let mut state = self.0.lock().unwrap();
        state.sender = sender;
        state.writable = writable;
        state.ready = false;
        state.binding += 1;
        RemoteTerminalFeed { control: self.clone(), binding: state.binding }
    }

    pub fn set_writable(&self, writable: bool) { self.0.lock().unwrap().writable = writable; }
    pub fn is_live(&self) -> bool { let s = self.0.lock().unwrap(); !s.closed && !s.exited }
}

/// A connection's frame producer. Rebinding invalidates this handle, so frames
/// from a cancelled old driver cannot overwrite a reconnected view's grid.
#[derive(Clone)]
pub struct RemoteTerminalFeed { control: RemoteTerminalControl, binding: u64 }
impl RemoteTerminalFeed {
    pub fn is_live(&self) -> bool {
        let s = self.control.0.lock().unwrap();
        s.binding == self.binding && !s.closed && !s.exited
    }
    pub fn gap(&self) {
        let mut s = self.control.0.lock().unwrap();
        if s.binding == self.binding { s.ready = false; }
    }

    pub fn replay(&self, cols: u16, rows: u16, replay: &[u8]) -> Result<()> {
        // Reject unreasonable grids instead of silently reflowing a snapshot
        // drawn at another size. Match the existing capture parser's limits.
        if !(1..=1024).contains(&cols) || !(1..=512).contains(&rows) {
            bail!("Host advertised an invalid terminal grid ({cols} × {rows})");
        }
        let mut s = self.control.0.lock().unwrap();
        if s.binding != self.binding || s.closed || s.exited { return Ok(()); }
        let payload = if let Some((captured_cols, captured_rows, payload)) = crate::parse_capture_header(replay) {
            if (captured_cols, captured_rows) != (cols, rows) {
                bail!("Host terminal replay dimensions disagree with its snapshot");
            }
            payload
        } else { replay };
        s.grid = TerminalState::new(cols, rows, 5000);
        s.grid.advance(payload);
        s.grid.clear_collected();
        s.events.clear();
        s.ready = true;
        s.emit(TerminalEvent::ScrollbackReset { id: REMOTE_SESSION });
        s.emit(TerminalEvent::Resize { id: REMOTE_SESSION, cols, rows });
        Ok(())
    }

    pub fn output(&self, bytes: &[u8]) {
        let mut s = self.control.0.lock().unwrap();
        if s.binding != self.binding || !s.ready || s.closed || s.exited { return; }
        let events = s.grid.advance_collecting(REMOTE_SESSION, bytes);
        for event in events {
            // Server CWDs stay opaque; automatic device replies are writes too.
            if matches!(event, TerminalEvent::CwdChanged { .. })
                || (!s.writable && matches!(event, TerminalEvent::PtyReply { .. })) { continue; }
            s.emit(event);
        }
        // No private agent sidebands or raw output enter local ambient storage.
        s.emit(TerminalEvent::Output { id: REMOTE_SESSION, bytes: Vec::new() });
    }

    pub fn exit(&self, code: Option<i32>) {
        let mut s = self.control.0.lock().unwrap();
        if s.binding != self.binding { return; }
        s.exited = true;
        s.ready = false;
        s.emit(TerminalEvent::Exit { id: REMOTE_SESSION, code });
    }
}

pub struct RemoteTerminalBackend(RemoteTerminalControl);
impl RemoteTerminalBackend {
    pub fn new() -> (Self, RemoteTerminalControl) {
        let control = RemoteTerminalControl(Arc::new(Mutex::new(State {
            grid: TerminalState::new(80, 24, 5000), binding: 0, events: VecDeque::new(),
            sender: None, writable: false, ready: false, closed: false, exited: false, waker: None,
        })));
        (Self(control.clone()), control)
    }
}

impl TerminalBackend for RemoteTerminalBackend {
    fn is_remote(&self) -> bool { true }
    fn can_resize(&self, _: TerminalSessionId) -> bool {
        let s = self.0.0.lock().unwrap();
        s.ready && s.writable && s.sender.is_some() && !s.closed && !s.exited
    }
    fn spawn(&mut self, _: SpawnConfig) -> Result<TerminalSessionId> { bail!("Remote terminal creation is unsupported") }
    fn write(&mut self, _: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        self.0.0.lock().unwrap().send(RemoteTerminalCommand::Input(bytes.to_vec()))
    }
    fn resize(&mut self, _: TerminalSessionId, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 { bail!("Invalid terminal size"); }
        self.0.0.lock().unwrap().send(RemoteTerminalCommand::Resize(cols, rows))
    }
    fn snapshot(&self, _: TerminalSessionId) -> Result<TerminalSnapshot> {
        let s = self.0.0.lock().unwrap();
        let mut snapshot = TerminalSnapshot::empty(80, 24);
        s.grid.fill_snapshot(&mut snapshot);
        Ok(snapshot)
    }
    fn input_mode(&self, _: TerminalSessionId) -> InputMode { self.0.0.lock().unwrap().grid.input_mode() }
    fn mouse_mode(&self, _: TerminalSessionId) -> MouseMode { self.0.0.lock().unwrap().grid.mouse_mode() }
    fn bracketed_paste(&self, _: TerminalSessionId) -> Result<bool> { Ok(self.0.0.lock().unwrap().grid.is_bracketed_paste()) }
    fn search_grid(&self, _: TerminalSessionId) -> Vec<Vec<Cell>> { self.0.0.lock().unwrap().grid.fill_search_grid() }
    fn scroll(&mut self, _: TerminalSessionId, delta: i32) -> Result<()> { self.0.0.lock().unwrap().grid.scroll_lines(delta); Ok(()) }
    fn scroll_to_bottom(&mut self, _: TerminalSessionId) -> Result<()> { self.0.0.lock().unwrap().grid.scroll_to_bottom(); Ok(()) }
    fn clear(&mut self, _: TerminalSessionId) -> Result<()> { self.0.0.lock().unwrap().grid.clear(); Ok(()) }
    fn set_output_waker(&mut self, _: TerminalSessionId, waker: OutputWaker) { self.0.0.lock().unwrap().waker = Some(waker); }
    fn drain_events(&mut self) -> Vec<TerminalEvent> { self.0.0.lock().unwrap().events.drain(..).collect() }
    fn close(&mut self, _: TerminalSessionId) -> Result<()> {
        let sender = {
            let mut s = self.0.0.lock().unwrap();
            if s.closed { return Ok(()); }
            s.closed = true;
            s.ready = false;
            s.sender.take()
        };
        if let Some(sender) = sender { sender(RemoteTerminalCommand::Detach)?; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_is_exact_read_only_is_enforced_and_close_only_detaches() {
        let (mut backend, control) = RemoteTerminalBackend::new();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let captured = commands.clone();
        let feed = control.bind(Some(Arc::new(move |c| { captured.lock().unwrap().push(c); Ok(()) })), true);
        assert!(backend.write(REMOTE_SESSION, b"before snapshot").is_err());
        feed.replay(5, 2, b"abcde12345").unwrap();
        feed.output(b"X");
        assert_eq!((backend.snapshot(REMOTE_SESSION).unwrap().cols, backend.snapshot(REMOTE_SESSION).unwrap().rows), (5, 2));
        feed.replay(5, 2, b"rest").unwrap();
        let snapshot = backend.snapshot(REMOTE_SESSION).unwrap();
        assert!(snapshot.rows_text(0, 2).contains("rest"));
        assert!(!snapshot.rows_text(0, 2).contains('X'), "replay replaces rather than appends");
        control.set_writable(false);
        assert!(!backend.can_resize(REMOTE_SESSION));
        assert!(backend.write(REMOTE_SESSION, b"no").is_err());
        assert!(backend.resize(REMOTE_SESSION, 100, 30).is_err());
        control.set_writable(true);
        backend.write(REMOTE_SESSION, b"yes").unwrap();
        backend.resize(REMOTE_SESSION, 100, 30).unwrap();
        assert_eq!(backend.snapshot(REMOTE_SESSION).unwrap().cols, 5, "resize awaits authoritative replay");
        feed.gap();
        assert!(backend.write(REMOTE_SESSION, b"gap").is_err());
        assert!(feed.replay(0, 2, b"").is_err());
        let replacement = control.bind(None, false);
        feed.replay(9, 3, b"stale").unwrap();
        feed.output(b"stale");
        feed.exit(Some(0));
        assert!(replacement.is_live(), "old connection exit cannot kill the new view");
        assert_eq!(backend.snapshot(REMOTE_SESSION).unwrap().cols, 5);
        control.bind(Some(Arc::new({ let captured = commands.clone(); move |c| { captured.lock().unwrap().push(c); Ok(()) } })), false);
        backend.close(REMOTE_SESSION).unwrap();
        backend.close(REMOTE_SESSION).unwrap();
        assert_eq!(*commands.lock().unwrap(), vec![RemoteTerminalCommand::Input(b"yes".to_vec()), RemoteTerminalCommand::Resize(100, 30), RemoteTerminalCommand::Detach]);
        assert!(backend.external_id_of(REMOTE_SESSION).is_none());
        assert!(backend.cwd_hint(REMOTE_SESSION).is_none());
        assert!(backend.serialize_buffer(REMOTE_SESSION, 1024).is_empty());
    }
}
