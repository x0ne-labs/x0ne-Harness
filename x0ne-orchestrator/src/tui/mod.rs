//! tui — the x0ne TUI. There is exactly one.
//!
//! Design goals (vs the retired `tui_mode` and the rustyline CLI):
//!
//!   * Persistent input zone at the bottom that never collides with
//!     streaming agent output above. Type while the agent works; lines
//!     stack up in the visible queue panel until you press Enter.
//!   * Live status bar with the three "bodies" of x0ne (runtime / vm /
//!     browser) rendered as dots whose color reflects whether each
//!     downstream is connected, in use, or idle.
//!   * A Tab-switchable side panel: conversation (default) / queued
//!     messages / snapshots (point-in-time restore) / async handles /
//!     parallel worlds. One pane, five views, all driven by the same
//!     process-wide state (`input_queue`, `goal::GoalManager`, etc.).
//!   * Slash commands inline (as in the CLI) but the bar at the bottom
//!     adapts contextually to the active panel.
//!   * Action trail: each tool the agent runs emits a typed line with
//!     a status icon (◆ snapshot / ✓ done / ⏳ streaming / ✗ failed /
//!     ⏸ paused). The trail scrolls under the conversation.
//!
//! Structure:
//!   * [`app::AppState`]  — single source of truth for the screen.
//!   * `render::draw`   — pure render function (no I/O).
//!   * `events::handle` — keyboard / resize / tick event router.
//!   * [`run_tui`]        — entrypoint, ties them together.
//!
//! Execution: the runner routes commands around a persistent
//! `CliAgentSession` — one canonical Driver turn per user message, the
//! blocking confirmation round-trip in `gate.rs`, frontend-claimed actions
//! in `driver_frontend.rs`. This is the ONE canonical rich TUI, reached by
//! `--tui` — the only flag that launches it.

pub mod app;
pub mod badge;
pub mod driver_frontend;
mod events;
/// Rendering harness for the T-B matrix — `AppState` → `TestBackend` cells.
#[cfg(test)]
mod frame_matrix;
pub mod gate;
pub mod intercept;
mod pty_term;
mod render;
pub mod runner;
pub mod wire;

pub use app::{AppState, ConversationLine, EnclaveStatus, LineKind, SidePanel, ToolStatus};
pub use wire::{AgentEvent, AskOption, TuiCommand};

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Entry point — set up the terminal, run the event loop, restore on exit.
/// `config` is the one `main` resolved: `-c`, flags, environment.
pub async fn run_tui(config: crate::config::Config) -> Result<()> {
    // Force `colored` to always emit ANSI escapes. In tui we route
    // every colored string through ansi-to-tui to build ratatui spans,
    // so we need the escapes to be present even when stdout looks
    // non-interactive to the `colored` heuristics (which happens once
    // crossterm puts us on the alternate screen). Without this the
    // `display::response_box` / `display::result_line` borders fall
    // back to plain text and lose their discreet-grey contour.
    colored::control::set_override(true);

    // Spawn the runner before bringing up the terminal so any boot
    // error message arrives as a normal AgentEvent::BootFailed line.
    let mut session = AgentSession::start(config);
    let mut terminal = init_terminal()?;

    let res = run_loop(&mut terminal, &mut session).await;

    // Signal runner to shut down before tearing the terminal down so
    // any final events have a chance to land.
    session.shutdown().await;

    restore_terminal(&mut terminal)?;
    res
}

/// One agent session: the screen state, its runner task, its goal poller.
///
/// Shared by `run_tui` and the agent pane of `windows`.
pub(crate) struct AgentSession {
    pub(crate) state: AppState,
    cmd_tx: tokio::sync::mpsc::Sender<TuiCommand>,
    runner: tokio::task::JoinHandle<()>,
    goal_reader: Option<crate::goal::GoalManager>,
    /// `None` = never reread: the panel's first display does not wait.
    last_goal_poll: Option<Instant>,
}

impl AgentSession {
    /// Spawns the runner on `config` and builds the state. Needs a tokio runtime.
    pub(crate) fn start(config: crate::config::Config) -> Self {
        let mut state = AppState::new();
        // The legacy TUI's ram/load strip, ported: metrics of the host this UI
        // runs on, polled in the background and drawn in the header.
        state.sys_state = Some(crate::sysmon::start_monitor(true));
        // The badges below and the runner read the same config: the one the
        // session runs on.
        //
        // Derived from the bridge configuration at startup, before any model
        // runs. Nothing the agent does can change it for the life of the
        // session, which is what makes it safe to hang a trust decision on.
        state.containment = crate::container_runtime::Containment::detect(
            &config.bridge.mode,
            config.bridge.sandbox_cmd.as_deref(),
        );
        state.cloud_state = Some(crate::cloudmon::start_monitor(config.cloud.enabled));
        // Read once at startup, like `containment`. See
        // `EnclaveStatus::for_session` for what it takes to be anything but
        // red, and why "a proof exists on this disk" is not enough.
        state.enclave = EnclaveStatus::for_session(
            crate::cc_runner::read_last_report().as_ref(),
            chrono::Utc::now(),
            &config,
        );
        // Channels: TUI ↔ runner. Bounded for backpressure on commands,
        // unbounded for events (the runner streams tokens at high rate and
        // must not block on a slow renderer).
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<TuiCommand>(32);
        let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let runner = tokio::spawn(runner::run_runner(cmd_rx, evt_tx, config));
        state.attach_channels(cmd_tx.clone(), evt_rx);
        Self {
            state,
            cmd_tx,
            runner,
            goal_reader: None,
            last_goal_poll: None,
        }
    }

    /// Drain the runner's events, advance animations, reread the goal.
    pub(crate) async fn tick(&mut self) {
        self.state.drain_agent_events();
        self.state.on_tick();
        refresh_goal_summary(
            &mut self.state,
            &mut self.goal_reader,
            &mut self.last_goal_poll,
        )
        .await;
    }

    /// Ask the runner to stop and give it 500 ms. Best-effort: if the channel
    /// is already closed (runner died), Shutdown is a no-op.
    pub(crate) async fn shutdown(self) {
        let _ = self.cmd_tx.send(TuiCommand::Shutdown).await;
        drop(self.cmd_tx);
        drop(self.state);
        let _ = tokio::time::timeout(Duration::from_millis(500), self.runner).await;
    }
}

/// Resize the embedded PTY to the area the agent is drawn in.
///
/// Heuristic, resynced each tick: vt100 absorbs small misalignments, and the
/// ioctl + SIGWINCH only fire when the dimensions actually change. If the
/// child exited (Ctrl+D on bash), drop the PtyTerm so the next Ctrl+S spawns
/// a fresh one.
fn sync_pty(state: &mut AppState, size: ratatui::layout::Size) {
    let shell_panel_showing = !state.shell_mode
        && !state.focus_mode
        && state.side_panel == crate::tui::app::SidePanel::Shell;
    if !(state.shell_mode || shell_panel_showing) {
        return;
    }
    let Some(pty) = state.pty.as_mut() else {
        return;
    };
    if pty.is_exited() {
        state.pty = None;
        state.shell_mode = false;
    } else if shell_panel_showing {
        // The Shell side panel: ~36% of the width minus the frame, body
        // height minus header/input/slash rows.
        let cols = ((size.width as usize * 36) / 100).saturating_sub(2) as u16;
        let rows = size.height.saturating_sub(9);
        pty.resize(rows, cols.max(20));
    } else {
        // Body area = full area minus header (5) and slash bar (1) and the
        // optional input row (~3).
        let rows = size.height.saturating_sub(9);
        let cols = size.width.saturating_sub(2);
        pty.resize(rows, cols);
    }
}

/// How often the screen rereads the goal from disk.
///
/// The UI tick is 80 ms; rereading a file eighty times a minute for a panel
/// nobody is looking at would be I/O noise. One second is enough: the goal
/// changes at the pace of turns.
const GOAL_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Fill [`AppState::goal_summary`] from what is actually persisted.
///
/// SECURITY: this is the ONLY producer of the field, and it reads
/// `GoalManager::get_active()` — the goal's file. No model action writes
/// here. A panel the model could paint would let it choose what the operator
/// believes they are monitoring; the operator would read "active" on a
/// blocked goal, and "three steps" on a goal that has burned forty.
///
/// The read only happens when the panel is on screen: out of view, there is
/// nothing to present, and cowork mode must not cost one read per second to
/// a session that does not use it.
async fn refresh_goal_summary(
    state: &mut AppState,
    manager: &mut Option<crate::goal::GoalManager>,
    last_poll: &mut Option<Instant>,
) {
    if state.side_panel != app::SidePanel::Goal {
        return;
    }
    if let Some(at) = last_poll {
        if at.elapsed() < GOAL_POLL_INTERVAL {
            return;
        }
    }
    *last_poll = Some(Instant::now());
    // The manager is mounted once and kept: `GoalStore::with_root` does a
    // `create_dir_all` on every construction, and redoing it once per second
    // would be a syscall for nothing.
    if manager.is_none() {
        *manager = crate::goal::GoalManager::new_default().ok();
    }
    // An unreadable store does not erase what is on screen: a snapshot from
    // one second ago is better than a panel that flickers empty.
    let Some(manager) = manager.as_ref() else {
        return;
    };
    if let Ok(active) = manager.get_active().await {
        state.goal_summary = active.as_ref().map(app::GoalSummary::of);
    }
}

async fn run_loop(terminal: &mut Tui, session: &mut AgentSession) -> Result<()> {
    let tick_rate = Duration::from_millis(80);
    let mut last_tick = Instant::now();

    loop {
        let state = &mut session.state;
        // Render the current state.
        // The renderer reports where each conversation row landed, so a click
        // can be resolved back to an action. Collected through an out-param
        // rather than by mutating `state` inside the draw closure: `draw`
        // stays a pure function of the state it is handed, which is what
        // makes it testable without a terminal.
        let mut hits = render::ConvHits::default();
        terminal.draw(|frame| render::draw(frame, state, &mut hits))?;
        state.conv_hits = hits;

        // ── Event drain (smooth live-resize critical) ──────────────
        //
        // Drain ALL pending events with zero timeout. Without this,
        // a fast resize drag (which emits a Resize per pixel) would
        // process ONE event then redraw, leaving N-1 events queued
        // and the screen showing intermediate sizes. Draining first
        // means we only ever paint the final post-drag dimensions.
        // Same for paste storms and key-repeat.
        let mut had_event = false;
        let mut quit = false;
        while event::poll(Duration::from_millis(0))? {
            had_event = true;
            match event::read()? {
                Event::Key(key) => {
                    if events::handle_key(key, state) {
                        quit = true;
                        break;
                    }
                }
                Event::Paste(text) => {
                    // Bracketed paste: insert verbatim into the input
                    // textarea WITHOUT triggering Enter-submit on
                    // embedded newlines.
                    events::handle_paste(&text, state);
                }
                Event::Mouse(mouse) => {
                    // Wheel → conversation scroll. Other mouse kinds
                    // (clicks, drags) are ignored for now.
                    events::handle_mouse(mouse, state);
                }
                Event::Resize(_, _) => {
                    // Force ratatui to rebuild its backing buffer at
                    // the new size BEFORE we draw again. autoresize
                    // is normally called from terminal.draw() but on
                    // very fast drags it can lag a frame; calling it
                    // here closes that window.
                    terminal.autoresize()?;
                }
                _ => {}
            }
        }
        if quit {
            break;
        }

        // ── Shell mode tick: PTY-driven, no suspend dance ─────────
        let size = terminal.size().unwrap_or(ratatui::layout::Size {
            width: 80,
            height: 24,
        });
        sync_pty(state, size);

        // ── Wait for either a tick deadline or new input ───────────
        //
        // If we just processed events, redraw immediately on the
        // next iteration (skip the wait). Otherwise sleep until
        // the tick deadline so the animation phase advances and
        // the runner channel gets drained.
        if !had_event {
            let timeout = tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_millis(0));
            // poll returns true if an event arrived during the wait —
            // we don't care about the value, we just want to wake up.
            let _ = event::poll(timeout)?;
        }

        if last_tick.elapsed() >= tick_rate {
            session.tick().await;
            last_tick = Instant::now();
        }

        if session.state.should_quit {
            break;
        }
    }
    Ok(())
}

fn init_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
    )?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
    )?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The Goal panel has only ONE producer, and it is a disk read.
    ///
    /// SECURITY: `AppState::goal_summary` is presentation only. The surfaces
    /// that could write it are exactly those that see an `AppState` — the
    /// files of `tui/` — and among them, `app.rs` (`apply_event`) and
    /// `events.rs` process messages coming from the agent. An `AgentEvent`
    /// that filled this field would give the model control over what the
    /// operator READS about the goal they are monitoring: "active" on a
    /// blocked goal, "three steps" on a goal that has burned forty. The field
    /// is therefore written only here, from `GoalManager::get_active()`.
    ///
    /// A failure means a second producer has appeared. It is not necessarily
    /// a flaw — but it is a security decision, and it must be made explicitly
    /// rather than discovered later.
    #[test]
    fn only_the_disk_fills_the_goal_panel() {
        // The files that see an `AppState` and receive from the agent.
        const FED_BY_THE_AGENT: [(&str, &str); 5] = [
            ("app.rs", include_str!("app.rs")),
            ("events.rs", include_str!("events.rs")),
            ("runner.rs", include_str!("runner.rs")),
            ("driver_frontend.rs", include_str!("driver_frontend.rs")),
            ("wire.rs", include_str!("wire.rs")),
        ];
        for (name, src) in FED_BY_THE_AGENT {
            assert!(
                !src.contains("goal_summary ="),
                "{name} writes to the Goal panel: the model can paint what \
                 the operator reads of their goal"
            );
        }
        // And the single producer is indeed here, exactly once. The pattern
        // is glued back together at compile time so that the line searching
        // for it does not count itself.
        let here = include_str!("mod.rs");
        assert_eq!(
            here.matches(concat!("state.goal_", "summary =")).count(),
            1,
            "the single producer of the Goal panel has moved or been duplicated"
        );
    }
}
