//! Terminal plumbing shared by the interactive chat and `unharnessed login`:
//! the colour decision, one stdin line reader for the whole process, and
//! Ctrl+C as an event instead of a kill.
//!
//! No terminal crate: lines come from the terminal's own line editing
//! (canonical mode on Unix, the console's on Windows), so the same code
//! runs on Linux, macOS and Windows.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

/// ANSI styling, on only when stdout is a terminal that renders it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Style {
    color: bool,
}

impl Style {
    /// Colours when stdout is a terminal and `NO_COLOR` is unset. On
    /// Windows, only in terminals known to render ANSI (Windows Terminal,
    /// VS Code, a `TERM` from a Unix-like shell): the old console prints
    /// the escape codes as text.
    pub(crate) fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let terminal = std::io::stdout().is_terminal();
        let renders_ansi = if cfg!(windows) {
            [
                "WT_SESSION",
                "TERM_PROGRAM",
                "TERM",
                "ANSICON",
                "ConEmuANSI",
            ]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        } else {
            std::env::var("TERM").map_or(true, |term| term != "dumb")
        };
        Style {
            color: terminal && !no_color && renders_ansi,
        }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub(crate) fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    pub(crate) fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    pub(crate) fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    pub(crate) fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    pub(crate) fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }

    pub(crate) fn cyan(self, text: &str) -> String {
        self.paint("36", text)
    }
}

/// One event from the person at the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Input {
    /// A line, without its line ending.
    Line(String),
    /// Ctrl+C.
    Interrupt,
    /// Ctrl+D (Ctrl+Z then Enter on Windows), or stdin closed.
    Eof,
}

/// The process's terminal input: one thread reads stdin line by line, one
/// task turns Ctrl+C into [`Input::Interrupt`]. Start it once, inside the
/// tokio runtime; a read left pending (a prompt that a browser sign-in
/// answered first) delivers its line to the next reader.
pub(crate) struct LineInput {
    rx: mpsc::UnboundedReceiver<Input>,
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl LineInput {
    pub(crate) fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let last_interrupt = Arc::new(AtomicU64::new(0));

        let interrupts = tx.clone();
        let stamp = Arc::clone(&last_interrupt);
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                stamp.store(now_ms(), Ordering::Relaxed);
                if interrupts.send(Input::Interrupt).is_err() {
                    break;
                }
            }
        });

        let spawned = std::thread::Builder::new()
            .name("uh-stdin".to_string())
            .spawn(move || read_lines(&tx, &last_interrupt));
        if spawned.is_err() {
            // No reader thread: the session sees the end of input at once.
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(Input::Eof);
            return LineInput { rx };
        }
        LineInput { rx }
    }

    /// The next event; [`Input::Eof`] forever once input ended.
    pub(crate) async fn next(&mut self) -> Input {
        self.rx.recv().await.unwrap_or(Input::Eof)
    }
}

fn read_lines(tx: &mpsc::UnboundedSender<Input>, last_interrupt: &AtomicU64) {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    loop {
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                // On Windows, Ctrl+C at a prompt also ends the console read
                // with no bytes. That is the interrupt, not the end of input:
                // give its event a moment to land, then read again.
                std::thread::sleep(Duration::from_millis(200));
                if now_ms().saturating_sub(last_interrupt.load(Ordering::Relaxed)) < 1_000 {
                    continue;
                }
                let _ = tx.send(Input::Eof);
                return;
            }
            Ok(_) => {
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                if tx.send(Input::Line(line)).is_err() {
                    return;
                }
            }
            // A line that is not UTF-8 is dropped, not the end of input.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::InvalidData
                ) => {}
            Err(_) => {
                // The same Windows Ctrl+C can surface as an aborted read.
                std::thread::sleep(Duration::from_millis(200));
                if now_ms().saturating_sub(last_interrupt.load(Ordering::Relaxed)) < 1_000 {
                    continue;
                }
                let _ = tx.send(Input::Eof);
                return;
            }
        }
    }
}

/// Print `text` with no line ending and flush it (a prompt).
pub(crate) fn prompt(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{text}");
    let _ = stdout.flush();
}

/// Turn the terminal's echo off (`false`) or back on, for a secret typed
/// at a prompt. Unix only, through `stty` on the terminal itself; elsewhere,
/// or when stdin is not a terminal, nothing changes and the caller says the
/// value will show. Returns whether echo was changed.
pub(crate) fn set_echo(on: bool) -> bool {
    if !cfg!(unix) || !std::io::stdin().is_terminal() {
        return false;
    }
    std::process::Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
