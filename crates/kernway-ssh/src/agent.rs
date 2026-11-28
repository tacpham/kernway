//! An ssh-agent client (draft-miller-ssh-agent): list the keys an agent holds and
//! have it sign — the private key never leaves the agent. Talks to `SSH_AUTH_SOCK`
//! (a Unix socket) on Linux and macOS, and to the Windows OpenSSH agent's named pipe
//! (`\\.\pipe\openssh-ssh-agent`) on Windows.

use crate::hostkey::PublicKey;
use crate::wire::{Reader, Writer};
use crate::SshError;

const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
const RSA_SHA2_256: u32 = 2;
const RSA_SHA2_512: u32 = 4;

/// Where an ssh-agent listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    path: String,
}

#[cfg(unix)]
type Stream = rt_net::unix::AsyncUnixStream;
#[cfg(windows)]
type Stream = rt_net::pipe::AsyncNamedPipe;

fn agent_err(m: impl Into<String>) -> SshError {
    SshError::Auth { allowed: vec![format!("(ssh-agent: {})", m.into())] }
}

impl Agent {
    /// The agent at `path`: a Unix socket path, or a pipe name on Windows.
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }

    /// The agent the environment points to: `SSH_AUTH_SOCK`, or on Windows the
    /// OpenSSH agent's pipe when that is unset.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        if let Some(p) = std::env::var_os("SSH_AUTH_SOCK").filter(|p| !p.is_empty()) {
            return Some(Self::new(p.to_string_lossy().into_owned()));
        }
        cfg!(windows).then(|| Self::new(r"\\.\pipe\openssh-ssh-agent"))
    }

    async fn call(&self, request: &[u8]) -> Result<Vec<u8>, SshError> {
        #[cfg(unix)]
        let mut s: Stream = rt_net::unix::AsyncUnixStream::connect(&self.path).await.map_err(|e| agent_err(format!("cannot reach {}: {e}", self.path)))?;
        #[cfg(windows)]
        let mut s: Stream = rt_net::pipe::AsyncNamedPipe::connect(&self.path).await.map_err(|e| agent_err(format!("cannot reach {}: {e}", self.path)))?;
        let mut framed = u32::try_from(request.len()).expect("small").to_be_bytes().to_vec();
        framed.extend_from_slice(request);
        s.write_all(&framed).await?;
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            if buf.len() >= 4 {
                let len = u32::from_be_bytes(buf[..4].try_into().expect("4 bytes")) as usize;
                if len > 256 * 1024 {
                    return Err(agent_err("an oversized reply"));
                }
                if buf.len() >= 4 + len {
                    return Ok(buf[4..4 + len].to_vec());
                }
            }
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                return Err(agent_err("the agent closed the connection"));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// The keys the agent holds, with their comments. Keys of a type this client
    /// cannot use (certificates, FIDO keys) are left out.
    pub async fn identities(&self) -> Result<Vec<(PublicKey, String)>, SshError> {
        let reply = self.call(&[REQUEST_IDENTITIES]).await?;
        if reply.first() != Some(&IDENTITIES_ANSWER) {
            return Err(agent_err("the agent refused to list its keys"));
        }
        let mut r = Reader::new(&reply[1..]);
        let n = r.u32()?;
        let mut out = Vec::new();
        for _ in 0..n {
            let blob = r.string()?;
            let comment = r.utf8()?;
            if let Ok(k) = PublicKey::from_blob(blob) {
                out.push((k, comment));
            }
        }
        Ok(out)
    }

    /// Have the agent sign `data` with `key`, using `alg`; the wire-form signature.
    pub(crate) async fn sign(&self, key: &PublicKey, alg: &str, data: &[u8]) -> Result<Vec<u8>, SshError> {
        let flags = match alg {
            "rsa-sha2-256" => RSA_SHA2_256,
            "rsa-sha2-512" => RSA_SHA2_512,
            _ => 0,
        };
        let mut w = Writer::msg(SIGN_REQUEST);
        w.string(key.to_blob()).string(data).u32(flags);
        let reply = self.call(&w.into_inner()).await?;
        match reply.first() {
            Some(&SIGN_RESPONSE) => Ok(Reader::new(&reply[1..]).string()?.to_vec()),
            Some(&FAILURE) => Err(agent_err("the agent refused to sign (a confirm-only key declined?)")),
            _ => Err(agent_err("an unexpected reply to a sign request")),
        }
    }
}
