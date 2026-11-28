//! Interactive consoles: a shell (or a command) on a pseudo-terminal, full duplex —
//! keystrokes go in while output streams out, as a terminal window (or a browser
//! terminal over a WebSocket) needs.
//!
//! A [`Console`] is a channel on an [`SshSession`](crate::SshSession): opened from a
//! session, many run side by side with SFTP and tunnels over one login; opened
//! from an [`SshClient`] it takes that connection for itself.

use crate::client::SshClient;
use crate::session::{Channel, ChannelEvent, Start};
use crate::SshError;

/// The terminal a console asks the server for (`pty-req`, RFC 4254 §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyConfig {
    /// The `TERM` value (`xterm-256color`).
    pub term: String,
    /// Width in characters.
    pub cols: u32,
    /// Height in rows.
    pub rows: u32,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self { term: "xterm-256color".into(), cols: 80, rows: 24 }
    }
}

/// What a console reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleEvent {
    /// Bytes the program wrote (with a terminal, stdout and stderr both arrive here).
    Output(Vec<u8>),
    /// Bytes written to standard error (only without a terminal).
    Stderr(Vec<u8>),
    /// The program ended: its exit status, or the signal that ended it.
    Exit {
        /// The exit status, when it exited normally.
        status: Option<u32>,
        /// The signal name (`TERM`, `KILL` …), when one ended it.
        signal: Option<String>,
    },
    /// The console is over; `Some` says why when the connection failed.
    Closed(Option<String>),
}

/// A running console. Dropping it closes the session channel.
pub struct Console {
    channel: Channel,
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Console")
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.channel.writer_ref().close();
    }
}

impl Console {
    pub(crate) fn over(channel: Channel) -> Self {
        Self { channel }
    }

    /// Send keystrokes (or any bytes) to the program.
    pub fn write(&self, data: impl Into<Vec<u8>>) -> Result<(), SshError> {
        self.channel.writer_ref().write(data)
    }

    /// The terminal window changed size (`window-change`).
    pub fn resize(&self, cols: u32, rows: u32) -> Result<(), SshError> {
        self.channel.writer_ref().resize(cols, rows)
    }

    /// Deliver a signal (`INT`, `TERM`, `KILL` …) to the program, where the server
    /// supports it (OpenSSH does). With a terminal, writing `\x03` is Ctrl-C.
    pub fn signal(&self, name: &str) -> Result<(), SshError> {
        self.channel.writer_ref().signal(name)
    }

    /// No more input (end of file on the program's stdin).
    pub fn send_eof(&self) -> Result<(), SshError> {
        self.channel.writer_ref().send_eof()
    }

    /// End the session.
    pub fn close(&self) -> Result<(), SshError> {
        self.channel.writer_ref().close()
    }

    /// The next event; `None` once [`ConsoleEvent::Closed`] has been delivered.
    pub async fn next(&mut self) -> Option<ConsoleEvent> {
        loop {
            return match self.channel.next().await? {
                ChannelEvent::Data(d) => Some(ConsoleEvent::Output(d)),
                ChannelEvent::Stderr(d) => Some(ConsoleEvent::Stderr(d)),
                ChannelEvent::Exit { status, signal } => Some(ConsoleEvent::Exit { status, signal }),
                ChannelEvent::Closed(why) => Some(ConsoleEvent::Closed(why)),
                ChannelEvent::Eof => continue,
            };
        }
    }
}

impl SshClient {
    /// An interactive login shell on a terminal of `pty`. The console takes this
    /// connection for itself; to run several consoles, SFTP and tunnels over one
    /// login, use [`into_session`](Self::into_session) and open them from the session.
    pub async fn console(self, pty: PtyConfig) -> Result<Console, SshError> {
        self.into_session().console(pty).await
    }

    /// Run `command` as a console: output streams as it is written, input goes in as
    /// you send it. With `pty`, on a terminal (programs then behave as for a person:
    /// colours, prompts, `top`); without, stdout and stderr stay apart.
    pub async fn console_exec(self, command: &str, pty: Option<PtyConfig>) -> Result<Console, SshError> {
        self.into_session().open_session_channel(pty, Start::Exec(command.to_owned())).await.map(Console::over)
    }
}
