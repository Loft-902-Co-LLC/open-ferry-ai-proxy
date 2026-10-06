// Ported with how Bubble Tea runs a program (NewProgram, WithAltScreen,
// Run, eventLoop, handleCommands; v1.3.10, MIT, see
// licenses/bubbletea-LICENSE).
// https://github.com/charmbracelet/bubbletea

//! Runs the app in the terminal: takes the terminal over (raw mode, the
//! alternate screen, bracketed paste, no cursor), feeds the app key
//! presses, pastes and size changes, runs its commands, and draws it after
//! each update; then gives the terminal back as it was.
//!
//! Deviations from upstream:
//! - The view is drawn into a ratatui buffer the size of the terminal: its
//!   last lines that fit, each cut to the width, as Bubble Tea's renderer
//!   shows a view. Bubble Tea writes the view's text itself.
//! - Copying writes an OSC 52 sequence to the terminal, asking it to set
//!   the clipboard. Bubble Tea leaves copying to the program, which in
//!   upstream writes to the system clipboard.
//! - Commands still running when the app quits are dropped.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::terminal::{
    self as term, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::ansi;
use crate::app::{App, Msg, Platform};
use crate::keys::Key;
use crate::tea::Cmd;

/// How often the input thread checks whether to stop.
const INPUT_POLL: Duration = Duration::from_millis(100);

/// The platform the terminal gives: `open_url` opens URLs, and copying
/// asks the terminal to.
pub(crate) fn platform(open_url: Arc<dyn Fn(&str) + Send + Sync>) -> Platform {
    Platform {
        open_url,
        copy: Arc::new(|text| copy_osc52(&mut io::stdout().lock(), text)),
    }
}

/// Asks the terminal to put `text` on the clipboard.
fn copy_osc52(out: &mut impl io::Write, text: &str) -> Result<(), String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    write!(out, "\u{1b}]52;c;{encoded}\u{7}")
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

/// Puts the terminal back as it was, even on a panic.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            cursor::Show
        );
        let _ = disable_raw_mode();
    }
}

/// Stops the input thread when dropped.
struct Input {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads terminal events on a thread of its own and sends them on as
/// messages.
fn spawn_input(tx: mpsc::UnboundedSender<Msg>) -> io::Result<Input> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let thread = thread::Builder::new()
        .name("open-ferry-tui-input".to_owned())
        .spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match event::poll(INPUT_POLL) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(_) => return,
                }
                let msg = match event::read() {
                    Ok(Event::Key(key)) => Key::from_crossterm(&key).map(Msg::Key),
                    Ok(Event::Paste(text)) => Some(Msg::Key(Key::pasted(&text))),
                    Ok(Event::Resize(width, height)) => Some(Msg::Resize {
                        width: i64::from(width),
                        height: i64::from(height),
                    }),
                    Ok(_) => None,
                    Err(_) => return,
                };
                if let Some(msg) = msg
                    && tx.send(msg).is_err()
                {
                    return;
                }
            }
        })?;
    Ok(Input {
        stop,
        thread: Some(thread),
    })
}

/// Runs `cmd`; true when it quits.
fn exec(cmd: Cmd, tx: &mpsc::UnboundedSender<Msg>, tasks: &mut JoinSet<()>) -> bool {
    match cmd {
        Cmd::Run(task) => {
            let tx = tx.clone();
            tasks.spawn(async move {
                if let Some(msg) = task.await {
                    let _ = tx.send(msg);
                }
            });
            false
        }
        Cmd::Batch(cmds) => {
            let mut quit = false;
            for cmd in cmds {
                quit |= exec(cmd, tx, tasks);
            }
            quit
        }
        Cmd::Tick(after, msg) => {
            let tx = tx.clone();
            tasks.spawn(async move {
                tokio::time::sleep(after).await;
                let _ = tx.send(*msg);
            });
            false
        }
        Cmd::Quit => true,
    }
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &App) -> io::Result<()> {
    let view = app.view();
    terminal.draw(|frame| {
        let area = frame.area();
        ansi::render(&view, area, frame.buffer_mut());
    })?;
    Ok(())
}

/// Runs `app` in the terminal until it quits.
pub(crate) async fn run(mut app: App) -> io::Result<()> {
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    let _input = spawn_input(tx.clone())?;

    let mut quit = app.init().is_some_and(|cmd| exec(cmd, &tx, &mut tasks));
    let (width, height) = term::size()?;
    let _ = tx.send(Msg::Resize {
        width: i64::from(width),
        height: i64::from(height),
    });

    while !quit {
        draw(&mut terminal, &app)?;
        let Some(msg) = rx.recv().await else {
            break;
        };
        let mut next = Some(msg);
        while let Some(msg) = next {
            if let Some(cmd) = app.update(msg) {
                quit |= exec(cmd, &tx, &mut tasks);
            }
            if quit {
                break;
            }
            next = rx.try_recv().ok();
        }
        while tasks.try_join_next().is_some() {}
    }
    tasks.abort_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: copying writes the text base64-encoded in an OSC 52
    // sequence.
    #[test]
    fn copies_with_osc52() {
        let mut out = Vec::new();
        copy_osc52(&mut out, "sk-test").unwrap();
        assert_eq!(out, b"\x1b]52;c;c2stdGVzdA==\x07");
    }
}
