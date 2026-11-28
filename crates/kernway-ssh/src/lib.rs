//! # kernway-ssh
//!
//! A self-written SSH 2 client and SFTP client on the Kernway runtime — fully async on
//! the shard's reactor, no tokio, no libssh. `ring` supplies the cryptographic
//! primitives; the protocol on top of them is this crate's:
//!
//! - transport (RFC 4253): `curve25519-sha256` / `ecdh-sha2-nistp256` / `-nistp384` key
//!   exchange with strict-kex (the Terrapin fix), `chacha20-poly1305@openssh.com` and
//!   `aes256-gcm@openssh.com` / `aes128-gcm@openssh.com`, rekeying either way;
//! - server authentication: `ssh-ed25519`, `ecdsa-sha2-nistp256/384`, `rsa-sha2-256/512`
//!   host keys checked against an OpenSSH `known_hosts` file (plain, `[host]:port` and
//!   hashed entries, `@revoked`) or a pinned `SHA256:` fingerprint;
//! - user authentication (RFC 4252 / 4256): password, public key (Ed25519, ECDSA, RSA
//!   keys in OpenSSH or PEM form, passphrase-protected ones too), an ssh-agent,
//!   keyboard-interactive;
//! - connection (RFC 4254): session channels with flow control, `exec`, and
//!   interactive [`Console`]s on a pseudo-terminal (resize, signals, full duplex);
//! - SFTP version 3 (the protocol OpenSSH speaks), with pipelined reads and writes;
//! - SCP (`scp -t` / `scp -f`): files and directory trees with modes and times.
//!
//! ```rust,ignore
//! let key = PrivateKey::from_pem(&key_file_bytes)?;
//! let mut ssh = SshClient::connect(SshConfig::new("files.example", "deploy")
//!     .auth(Auth::Key(key))
//!     .host_key(HostKeyPolicy::KnownHosts(known_hosts_path))).await?;
//! let out = ssh.exec("uname -a").await?;
//! let mut sftp = ssh.sftp().await?;
//! sftp.write("upload/report.csv", &bytes).await?;
//! ```

#![forbid(unsafe_code)]

mod agent;
mod cipher;
mod client;
mod console;
mod hostkey;
mod kdf;
mod keys;
mod scp;
mod sftp;
mod transport;
mod wire;

pub use hostkey::{Curve, HostKeyPolicy, PublicKey};
pub use agent::Agent;
pub use client::{Auth, ExecOutput, SshClient, SshConfig};
pub use console::{Console, ConsoleEvent, PtyConfig};
pub use keys::PrivateKey;
pub use scp::ScpFile;
pub use sftp::{DirEntry, File, FsStats, Metadata, OpenOptions, Sftp};
pub use transport::Negotiated;
use std::fmt;

/// Something went wrong talking SSH or SFTP.
#[derive(Debug)]
pub enum SshError {
    /// A socket error (the connection failed or broke).
    Io(std::io::Error),
    /// The peer broke the protocol, or a packet failed its integrity check.
    Protocol(String),
    /// The server ended the session (RFC 4253 §11.1 reason code and text).
    Disconnected {
        /// The reason code.
        reason: u32,
        /// The server's explanation.
        description: String,
    },
    /// No algorithm of a kind both sides support (`kind`: kex, host key, cipher …).
    NoCommonAlgorithm {
        /// Which list found no match.
        kind: &'static str,
        /// What the server offered for it.
        server: Vec<String>,
    },
    /// The server's host key is not the one expected — unknown, changed, revoked,
    /// or its signature over the key exchange is wrong. Never ignore this.
    HostKey(String),
    /// Every authentication method tried was refused; `allowed` is what the
    /// server would still accept.
    Auth {
        /// The methods the server still accepts.
        allowed: Vec<String>,
    },
    /// A private key could not be read (format, passphrase, unsupported type).
    Key(String),
    /// A channel could not be opened, or a request on it was refused.
    Channel(String),
    /// An SFTP request failed with this status (see [`SftpStatus`]).
    Sftp {
        /// The status code.
        status: SftpStatus,
        /// The server's message.
        message: String,
    },
    /// The connect or an operation took longer than its timeout.
    Timeout,
}

impl fmt::Display for SshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SshError::Io(e) => write!(f, "ssh: io: {e}"),
            SshError::Protocol(m) => write!(f, "ssh: protocol: {m}"),
            SshError::Disconnected { reason, description } => write!(f, "ssh: disconnected by the server ({reason}): {description}"),
            SshError::NoCommonAlgorithm { kind, server } => write!(f, "ssh: no common {kind} algorithm (the server offers {})", server.join(", ")),
            SshError::HostKey(m) => write!(f, "ssh: host key: {m}"),
            SshError::Auth { allowed } => write!(f, "ssh: authentication failed (the server allows: {})", allowed.join(", ")),
            SshError::Key(m) => write!(f, "ssh: private key: {m}"),
            SshError::Channel(m) => write!(f, "ssh: channel: {m}"),
            SshError::Sftp { status, message } => write!(f, "sftp: {status:?}: {message}"),
            SshError::Timeout => write!(f, "ssh: timed out"),
        }
    }
}

impl std::error::Error for SshError {}

impl From<std::io::Error> for SshError {
    fn from(e: std::io::Error) -> Self {
        SshError::Io(e)
    }
}

/// An SFTP status code (draft-ietf-secsh-filexfer-02 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpStatus {
    /// `SSH_FX_EOF`: read past the end.
    Eof,
    /// `SSH_FX_NO_SUCH_FILE`.
    NoSuchFile,
    /// `SSH_FX_PERMISSION_DENIED`.
    PermissionDenied,
    /// `SSH_FX_FAILURE`: anything else the server could not do (e.g. a directory not empty).
    Failure,
    /// `SSH_FX_BAD_MESSAGE`.
    BadMessage,
    /// `SSH_FX_OP_UNSUPPORTED`.
    Unsupported,
    /// A code outside version 3.
    Other(u32),
}

impl SftpStatus {
    pub(crate) fn from_code(code: u32) -> Self {
        match code {
            1 => SftpStatus::Eof,
            2 => SftpStatus::NoSuchFile,
            3 => SftpStatus::PermissionDenied,
            4 => SftpStatus::Failure,
            5 => SftpStatus::BadMessage,
            8 => SftpStatus::Unsupported,
            c => SftpStatus::Other(c),
        }
    }
}

impl SshError {
    /// Whether this is SFTP's "no such file".
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, SshError::Sftp { status: SftpStatus::NoSuchFile, .. })
    }
}
