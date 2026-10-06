// Ported with how Bubble Tea runs commands (Cmd, Batch, Tick, Quit; v1.3.10,
// MIT, see licenses/bubbletea-LICENSE).
// https://github.com/charmbracelet/bubbletea

//! Commands: the work an update asks to have done next, whose result comes
//! back as a message.
//!
//! Deviations from upstream:
//! - A command is a future the runtime spawns on the tokio runtime, rather
//!   than a function Bubble Tea calls on a goroutine of its own. A future
//!   that is dropped (when the TUI quits) stops; a goroutine can't be
//!   stopped.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::app::Msg;

/// A future that ends in a message, or in none.
pub(crate) type Task = Pin<Box<dyn Future<Output = Option<Msg>> + Send>>;

/// A command (Bubble Tea's `tea.Cmd`).
pub(crate) enum Cmd {
    /// Runs the future and sends its message.
    Run(Task),
    /// Runs the commands at once (`tea.Batch`).
    Batch(Vec<Cmd>),
    /// Sends the message after the delay (`tea.Tick`).
    Tick(Duration, Box<Msg>),
    /// Ends the program (`tea.Quit`).
    Quit,
}

impl Cmd {
    /// A command that runs `task`.
    pub(crate) fn run(task: impl Future<Output = Option<Msg>> + Send + 'static) -> Self {
        Self::Run(Box::pin(task))
    }
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run(_) => f.write_str("Run(..)"),
            Self::Batch(cmds) => f.debug_tuple("Batch").field(cmds).finish(),
            Self::Tick(after, _) => f.debug_tuple("Tick").field(after).finish(),
            Self::Quit => f.write_str("Quit"),
        }
    }
}

/// `tea.Batch`: no command when none are given, the one when one is, else
/// a batch of them.
pub(crate) fn batch(cmds: impl IntoIterator<Item = Option<Cmd>>) -> Option<Cmd> {
    let mut cmds: Vec<Cmd> = cmds.into_iter().flatten().collect();
    if cmds.len() > 1 {
        Some(Cmd::Batch(cmds))
    } else {
        cmds.pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: a batch drops missing commands, as tea.Batch drops nil
    // ones.
    #[test]
    fn batches_as_bubble_tea_does() {
        assert!(batch([None, None]).is_none());
        assert!(matches!(batch([None, Some(Cmd::Quit)]), Some(Cmd::Quit)));
        let both = batch([Some(Cmd::Quit), None, Some(Cmd::Quit)]);
        assert!(matches!(both, Some(Cmd::Batch(ref cmds)) if cmds.len() == 2));
        assert_eq!(format!("{both:?}"), "Some(Batch([Quit, Quit]))");
    }
}
