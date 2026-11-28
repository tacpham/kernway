//! Interactive consoles: a shell (or a command) on a pseudo-terminal, full duplex —
//! keystrokes go in while output streams out, as a terminal window (or a browser
//! terminal over a WebSocket) needs.
//!
//! A [`Console`] owns its connection: a task on the current shard drives it, taking
//! [`Console::write`] / [`resize`](Console::resize) commands and delivering
//! [`ConsoleEvent`]s from [`Console::next`]. It waits on the network and on the
//! application at once; the transport's reads are cancel-safe, so whichever comes
//! first wins and nothing in flight is lost.

use crate::client::SshClient;
use crate::SshError;
use rt_core::{mailbox, MailboxReceiver, MailboxSender};
use std::future::Future;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

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

enum Command {
    Data(Vec<u8>),
    Resize(u32, u32),
    Signal(String),
    Eof,
    Close,
}

/// A running console. Dropping it closes the session.
pub struct Console {
    commands: MailboxSender<Command>,
    events: MailboxReceiver<ConsoleEvent>,
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Console")
    }
}

fn gone() -> SshError {
    SshError::Channel("the console has ended".into())
}

impl Console {
    fn send(&self, c: Command) -> Result<(), SshError> {
        self.commands.try_send(c).map_err(|e| match e {
            rt_core::TrySendError::Full(_) => SshError::Channel("the console is not keeping up with input".into()),
            rt_core::TrySendError::Closed(_) => gone(),
        })
    }

    /// Send keystrokes (or any bytes) to the program.
    pub fn write(&self, data: impl Into<Vec<u8>>) -> Result<(), SshError> {
        self.send(Command::Data(data.into()))
    }

    /// The terminal window changed size (`window-change`).
    pub fn resize(&self, cols: u32, rows: u32) -> Result<(), SshError> {
        self.send(Command::Resize(cols, rows))
    }

    /// Deliver a signal (`INT`, `TERM`, `KILL` …) to the program, where the server
    /// supports it (OpenSSH does). With a terminal, writing `\x03` is Ctrl-C.
    pub fn signal(&self, name: &str) -> Result<(), SshError> {
        self.send(Command::Signal(name.to_owned()))
    }

    /// No more input (end of file on the program's stdin).
    pub fn send_eof(&self) -> Result<(), SshError> {
        self.send(Command::Eof)
    }

    /// End the session.
    pub fn close(&self) -> Result<(), SshError> {
        self.send(Command::Close)
    }

    /// The next event; `None` once [`ConsoleEvent::Closed`] has been delivered.
    pub async fn next(&mut self) -> Option<ConsoleEvent> {
        self.events.recv().await
    }
}

/// The RFC 4254 §8 terminal modes we send: just the end marker (the server's defaults).
const TTY_OP_END: u8 = 0;

impl SshClient {
    /// An interactive login shell on a terminal of `pty`. The console takes over this
    /// connection (open another one for SFTP alongside).
    pub async fn console(self, pty: PtyConfig) -> Result<Console, SshError> {
        self.start_console(None, Some(pty)).await
    }

    /// Run `command` as a console: output streams as it is written, input goes in as
    /// you send it. With `pty`, on a terminal (programs then behave as for a person:
    /// colours, prompts, `top`); without, stdout and stderr stay apart.
    pub async fn console_exec(self, command: &str, pty: Option<PtyConfig>) -> Result<Console, SshError> {
        self.start_console(Some(command), pty).await
    }

    async fn start_console(mut self, command: Option<&str>, pty: Option<PtyConfig>) -> Result<Console, SshError> {
        let id = self.open_session().await?;
        if let Some(p) = &pty {
            let ok = self
                .request(id, "pty-req", |w| {
                    w.string(&p.term).u32(p.cols).u32(p.rows).u32(0).u32(0).string([TTY_OP_END]);
                })
                .await?;
            if !ok {
                return Err(SshError::Channel("the server refused a terminal (pty-req)".into()));
            }
        }
        let ok = match command {
            Some(c) => {
                self.request(id, "exec", |w| {
                    w.string(c);
                })
                .await?
            }
            None => self.request(id, "shell", |_| {}).await?,
        };
        if !ok {
            return Err(SshError::Channel("the server refused to start the session".into()));
        }
        let (command_tx, command_rx) = mailbox(1024);
        let (event_tx, event_rx) = mailbox(4096);
        rt_core::spawn(drive(self, id, command_rx, event_tx));
        Ok(Console { commands: command_tx, events: event_rx })
    }
}

enum Woke {
    Network(Result<(), SshError>),
    Command(Option<Command>),
    Retry,
}

/// The console's task: deliver what arrived, then wait for the network, a command,
/// or (when the application is behind) a short pause.
async fn drive(mut ssh: SshClient, id: u32, mut commands: MailboxReceiver<Command>, events: MailboxSender<ConsoleEvent>) {
    let mut held: Vec<ConsoleEvent> = Vec::new();
    let result: Result<(), SshError> = async {
        loop {
            while let Some(p) = ssh.t.recv_buffered().await? {
                ssh.dispatch(p).await?;
            }
            if held.is_empty() {
                let (out, err, ended) = ssh.take_output(id).await?;
                if !out.is_empty() {
                    held.push(ConsoleEvent::Output(out));
                }
                if !err.is_empty() {
                    held.push(ConsoleEvent::Stderr(err));
                }
                if let Some((status, signal)) = ended {
                    held.push(ConsoleEvent::Exit { status, signal });
                }
            }
            // Hand the events over; what does not fit waits (and so does the window).
            while !held.is_empty() {
                let e = held.remove(0);
                let last = matches!(e, ConsoleEvent::Exit { .. });
                match events.try_send(e) {
                    Ok(()) if last => return Ok(()),
                    Ok(()) => {}
                    Err(rt_core::TrySendError::Full(e)) => {
                        held.insert(0, e);
                        break;
                    }
                    Err(rt_core::TrySendError::Closed(_)) => return Ok(()),
                }
            }
            let behind = !held.is_empty();
            let woke = {
                let mut net = pin!(ssh.t.fill());
                let mut cmd = pin!(commands.recv());
                let mut pause = pin!(rt_core::sleep(Duration::from_millis(10)));
                std::future::poll_fn(|cx| {
                    if !behind {
                        if let Poll::Ready(r) = net.as_mut().poll(cx) {
                            return Poll::Ready(Woke::Network(r));
                        }
                    }
                    if let Poll::Ready(c) = cmd.as_mut().poll(cx) {
                        return Poll::Ready(Woke::Command(c));
                    }
                    if behind && pause.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Woke::Retry);
                    }
                    Poll::Pending
                })
                .await
            };
            match woke {
                Woke::Network(r) => r?,
                Woke::Retry => {}
                Woke::Command(None) | Woke::Command(Some(Command::Close)) => {
                    ssh.close(id).await?;
                    return Ok(());
                }
                Woke::Command(Some(Command::Data(d))) => ssh.write(id, &d).await?,
                Woke::Command(Some(Command::Resize(cols, rows))) => {
                    ssh.request_no_reply(id, "window-change", |w| {
                        w.u32(cols).u32(rows).u32(0).u32(0);
                    })
                    .await?;
                }
                Woke::Command(Some(Command::Signal(name))) => {
                    ssh.request_no_reply(id, "signal", |w| {
                        w.string(&name);
                    })
                    .await?;
                }
                Woke::Command(Some(Command::Eof)) => ssh.send_eof(id).await?,
            }
        }
    }
    .await;
    let _ = events.try_send(ConsoleEvent::Closed(result.err().map(|e| e.to_string())));
}
