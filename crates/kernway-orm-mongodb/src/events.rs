//! Command monitoring, as the Command Logging and Monitoring spec defines it:
//! every command the client sends for an operation (not the connection
//! handshake, not the monitors' `hello`s) is reported to the listeners set
//! with [`Client::on_command`](crate::Client::on_command) — a started event,
//! then a succeeded or failed one. Sensitive commands (authentication, user
//! management, a `hello` carrying speculative authentication) are reported
//! with an empty command and reply.

use crate::bson::{Bson, Document, ObjectId};
use crate::error::Error;
use crate::options::Address;
use std::time::Duration;

/// Where a command ran and which one it was (common to all three events).
#[derive(Debug, Clone, PartialEq)]
pub struct CommandContext {
    /// The command's name (its first key).
    pub command_name: String,
    /// The database it ran against.
    pub database_name: String,
    /// The OP_MSG request id.
    pub request_id: i32,
    /// The operation it belongs to (shared by an operation's retries,
    /// getMores and killCursors).
    pub operation_id: i32,
    /// The server.
    pub address: Address,
    /// The driver's id for the connection.
    pub connection_id: u64,
    /// The server's id for the connection (`connectionId` in its hello).
    pub server_connection_id: Option<i64>,
    /// The service behind a load balancer.
    pub service_id: Option<ObjectId>,
}

/// One monitoring event.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandEvent {
    /// About to send `command` (document sequences folded into arrays).
    Started {
        /// The command as sent.
        command: Document,
        /// Where and which.
        context: CommandContext,
    },
    /// The server answered `ok: 1` (write errors included).
    Succeeded {
        /// The reply.
        reply: Document,
        /// Round trip.
        duration: Duration,
        /// Where and which.
        context: CommandContext,
    },
    /// The command failed: `ok: 0`, or a network error.
    Failed {
        /// Why.
        failure: Error,
        /// Time until the failure.
        duration: Duration,
        /// Where and which.
        context: CommandContext,
    },
}

impl CommandEvent {
    /// Where and which.
    pub fn context(&self) -> &CommandContext {
        match self {
            CommandEvent::Started { context, .. } | CommandEvent::Succeeded { context, .. } | CommandEvent::Failed { context, .. } => context,
        }
    }
}

const SENSITIVE: [&str; 9] = ["authenticate", "saslstart", "saslcontinue", "getnonce", "createuser", "updateuser", "copydbgetnonce", "copydbsaslstart", "copydb"];

/// Whether a command (and its reply) must be reported empty.
pub(crate) fn is_sensitive(name: &str, command: &Document) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE.contains(&lower.as_str()) || (matches!(lower.as_str(), "hello" | "ismaster") && command.contains_key("speculativeAuthenticate"))
}

/// The command as the server sees it: `$db` added, each document sequence
/// folded into the command as an array.
pub(crate) fn as_sent(db: &str, command: &Document, seqs: &[crate::wire::Sequence<'_>]) -> Document {
    let mut c = command.clone();
    c.insert("$db", db);
    for s in seqs {
        c.insert(s.identifier, Bson::Array(s.documents.iter().cloned().map(Bson::Document).collect()));
    }
    c
}
