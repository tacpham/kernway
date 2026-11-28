//! SFTP version 3 (draft-ietf-secsh-filexfer-02 — the version OpenSSH speaks) over
//! an SSH `sftp` subsystem channel, with OpenSSH's extensions where the server has
//! them (`posix-rename@openssh.com`, `statvfs@openssh.com`, `fsync@openssh.com`,
//! `hardlink@openssh.com`, `limits@openssh.com`). Whole-file reads and writes keep
//! many requests in flight, so a transfer is not one round trip per chunk.

use crate::client::SshClient;
use crate::wire::{Reader, Writer};
use crate::{SftpStatus, SshError};
use std::collections::{BTreeMap, HashMap};

const FXP_INIT: u8 = 1;
const FXP_VERSION: u8 = 2;
const FXP_OPEN: u8 = 3;
const FXP_CLOSE: u8 = 4;
const FXP_READ: u8 = 5;
const FXP_WRITE: u8 = 6;
const FXP_LSTAT: u8 = 7;
const FXP_FSTAT: u8 = 8;
const FXP_SETSTAT: u8 = 9;
const FXP_OPENDIR: u8 = 11;
const FXP_READDIR: u8 = 12;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_REALPATH: u8 = 16;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_READLINK: u8 = 19;
const FXP_SYMLINK: u8 = 20;
const FXP_STATUS: u8 = 101;
const FXP_HANDLE: u8 = 102;
const FXP_DATA: u8 = 103;
const FXP_NAME: u8 = 104;
const FXP_ATTRS: u8 = 105;
const FXP_EXTENDED: u8 = 200;
const FXP_EXTENDED_REPLY: u8 = 201;

const ATTR_SIZE: u32 = 0x1;
const ATTR_UIDGID: u32 = 0x2;
const ATTR_PERMISSIONS: u32 = 0x4;
const ATTR_ACMODTIME: u32 = 0x8;
const ATTR_EXTENDED: u32 = 0x8000_0000;

const FXF_READ: u32 = 0x1;
const FXF_WRITE: u32 = 0x2;
const FXF_APPEND: u32 = 0x4;
const FXF_CREAT: u32 = 0x8;
const FXF_TRUNC: u32 = 0x10;
const FXF_EXCL: u32 = 0x20;

/// The largest SFTP packet accepted from the server.
const MAX_SFTP_PACKET: usize = 1 << 20;
/// Bytes per read / write request, and how many are in flight at once.
const CHUNK: u32 = 64 * 1024;
const IN_FLIGHT: usize = 16;

/// A file's attributes; a field is `None` when the server did not send it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// Size in bytes.
    pub size: Option<u64>,
    /// Owner user id.
    pub uid: Option<u32>,
    /// Owner group id.
    pub gid: Option<u32>,
    /// POSIX mode: the file type bits and the permission bits.
    pub permissions: Option<u32>,
    /// Last access, seconds since the Unix epoch.
    pub atime: Option<u32>,
    /// Last modification, seconds since the Unix epoch.
    pub mtime: Option<u32>,
}

impl Metadata {
    fn kind(&self) -> u32 {
        self.permissions.unwrap_or(0) & 0o170_000
    }

    /// A directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.kind() == 0o040_000
    }

    /// A regular file.
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.kind() == 0o100_000
    }

    /// A symbolic link (only [`Sftp::symlink_metadata`] reports these).
    #[must_use]
    pub fn is_symlink(&self) -> bool {
        self.kind() == 0o120_000
    }

    /// The permission bits alone (`0o644`).
    #[must_use]
    pub fn mode(&self) -> Option<u32> {
        self.permissions.map(|p| p & 0o7777)
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, SshError> {
        let flags = r.u32()?;
        let mut m = Metadata::default();
        if flags & ATTR_SIZE != 0 {
            m.size = Some(r.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            m.uid = Some(r.u32()?);
            m.gid = Some(r.u32()?);
        }
        if flags & ATTR_PERMISSIONS != 0 {
            m.permissions = Some(r.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            m.atime = Some(r.u32()?);
            m.mtime = Some(r.u32()?);
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..r.u32()? {
                r.string()?;
                r.string()?;
            }
        }
        Ok(m)
    }

    fn write(&self, w: &mut Writer) {
        let mut flags = 0;
        if self.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if self.uid.is_some() && self.gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if self.permissions.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if self.atime.is_some() && self.mtime.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        w.u32(flags);
        if let Some(s) = self.size {
            w.u64(s);
        }
        if let (Some(u), Some(g)) = (self.uid, self.gid) {
            w.u32(u).u32(g);
        }
        if let Some(p) = self.permissions {
            w.u32(p);
        }
        if let (Some(a), Some(m)) = (self.atime, self.mtime) {
            w.u32(a).u32(m);
        }
    }
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The file name (no directory part).
    pub name: String,
    /// Its attributes (as `lstat` sees it: a link is a link).
    pub metadata: Metadata,
    /// The `ls -l`-style line the server made for it.
    pub long_name: String,
}

/// File system statistics (`statvfs@openssh.com`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    /// Fundamental block size.
    pub block_size: u64,
    /// Total blocks.
    pub blocks: u64,
    /// Free blocks.
    pub blocks_free: u64,
    /// Blocks free to a non-root user.
    pub blocks_available: u64,
    /// Longest file name.
    pub name_max: u64,
}

impl FsStats {
    /// Bytes free to a non-root user.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.blocks_available.saturating_mul(self.block_size)
    }
}

/// How to open a file (like `std::fs::OpenOptions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenOptions {
    /// Open for reading.
    pub read: bool,
    /// Open for writing.
    pub write: bool,
    /// Create it if missing.
    pub create: bool,
    /// Create it, failing if it exists.
    pub create_new: bool,
    /// Cut it to length 0.
    pub truncate: bool,
    /// Writes go to the end.
    pub append: bool,
}

impl OpenOptions {
    fn flags(self) -> u32 {
        let mut f = 0;
        if self.read {
            f |= FXF_READ;
        }
        if self.write || self.append {
            f |= FXF_WRITE;
        }
        if self.append {
            f |= FXF_APPEND;
        }
        if self.create || self.create_new {
            f |= FXF_CREAT;
        }
        if self.create_new {
            f |= FXF_EXCL;
        }
        if self.truncate {
            f |= FXF_TRUNC;
        }
        f
    }
}

/// An open remote file. Close it with [`Sftp::close`].
#[derive(Debug)]
pub struct File {
    handle: Vec<u8>,
}

/// An SFTP session over an SSH connection.
pub struct Sftp {
    ssh: SshClient,
    chan: u32,
    next_id: u32,
    /// Channel bytes not yet a whole SFTP packet.
    buf: Vec<u8>,
    /// Replies that arrived while waiting for another one.
    stash: HashMap<u32, (u8, Vec<u8>)>,
    version: u32,
    extensions: Vec<(String, String)>,
    openssh: bool,
}

impl std::fmt::Debug for Sftp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sftp").field("version", &self.version).field("extensions", &self.extensions).finish_non_exhaustive()
    }
}

fn status_error(p: &[u8]) -> SshError {
    let mut r = Reader::new(p);
    let code = r.u32().unwrap_or(4);
    let message = r.utf8().unwrap_or_default();
    SshError::Sftp { status: SftpStatus::from_code(code), message }
}

fn unexpected(t: u8) -> SshError {
    SshError::Protocol(format!("an unexpected SFTP reply (type {t})"))
}

impl SshClient {
    /// Start SFTP on this connection (the `sftp` subsystem). The returned [`Sftp`] owns
    /// the connection; [`Sftp::ssh`] still runs commands on it.
    pub async fn sftp(mut self) -> Result<Sftp, SshError> {
        let chan = self.open_session().await?;
        if !self.request(chan, "subsystem", |w| {
            w.string("sftp");
        })
        .await?
        {
            return Err(SshError::Channel("the server has no sftp subsystem".into()));
        }
        let openssh = self.server_version().contains("OpenSSH");
        let mut s = Sftp { ssh: self, chan, next_id: 1, buf: Vec::new(), stash: HashMap::new(), version: 0, extensions: Vec::new(), openssh };
        let mut w = Writer::new();
        w.u8(FXP_INIT).u32(3);
        s.send_packet(w.into_inner()).await?;
        let (t, body) = s.read_packet().await?;
        if t != FXP_VERSION {
            return Err(unexpected(t));
        }
        let mut r = Reader::new(&body);
        s.version = r.u32()?;
        while !r.is_empty() {
            s.extensions.push((r.utf8()?, r.utf8()?));
        }
        Ok(s)
    }
}

impl Sftp {
    /// The protocol version the server agreed to (3).
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether the server announced an extension (`posix-rename@openssh.com` …).
    #[must_use]
    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|(n, _)| n == name)
    }

    /// The SSH connection underneath (to run commands alongside).
    pub fn ssh(&mut self) -> &mut SshClient {
        &mut self.ssh
    }

    /// End SFTP and get the connection back.
    pub async fn into_ssh(mut self) -> Result<SshClient, SshError> {
        self.ssh.close(self.chan).await?;
        Ok(self.ssh)
    }

    // ── packets ──────────────────────────────────────────────────────────

    async fn send_packet(&mut self, body: Vec<u8>) -> Result<(), SshError> {
        let mut framed = Vec::with_capacity(4 + body.len());
        framed.extend_from_slice(&u32::try_from(body.len()).map_err(|_| SshError::Protocol("an SFTP packet over 4 GiB".into()))?.to_be_bytes());
        framed.extend_from_slice(&body);
        self.ssh.write(self.chan, &framed).await
    }

    async fn read_packet(&mut self) -> Result<(u8, Vec<u8>), SshError> {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_be_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
                if len == 0 || len > MAX_SFTP_PACKET {
                    return Err(SshError::Protocol(format!("a bad SFTP packet length {len}")));
                }
                if self.buf.len() >= 4 + len {
                    let p: Vec<u8> = self.buf.drain(..4 + len).collect();
                    return Ok((p[4], p[5..].to_vec()));
                }
            }
            let more = self.ssh.read(self.chan, usize::MAX).await?;
            if more.is_empty() {
                return Err(SshError::Channel("the server ended the SFTP session".into()));
            }
            self.buf.extend(more);
        }
    }

    /// Send a request of type `t` (an id is prepended); returns the id.
    async fn send(&mut self, t: u8, body: impl FnOnce(&mut Writer)) -> Result<u32, SshError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let mut w = Writer::new();
        w.u8(t).u32(id);
        body(&mut w);
        self.send_packet(w.into_inner()).await?;
        Ok(id)
    }

    /// The next reply, whichever request it answers.
    async fn reply_any(&mut self) -> Result<(u32, u8, Vec<u8>), SshError> {
        if let Some(&id) = self.stash.keys().next() {
            let (t, b) = self.stash.remove(&id).expect("present");
            return Ok((id, t, b));
        }
        let (t, body) = self.read_packet().await?;
        if body.len() < 4 {
            return Err(SshError::Protocol("an SFTP reply without an id".into()));
        }
        let id = u32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
        Ok((id, t, body[4..].to_vec()))
    }

    /// The reply to request `id`.
    async fn reply(&mut self, id: u32) -> Result<(u8, Vec<u8>), SshError> {
        if let Some(r) = self.stash.remove(&id) {
            return Ok(r);
        }
        loop {
            let (t, body) = self.read_packet().await?;
            if body.len() < 4 {
                return Err(SshError::Protocol("an SFTP reply without an id".into()));
            }
            let got = u32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
            if got == id {
                return Ok((t, body[4..].to_vec()));
            }
            self.stash.insert(got, (t, body[4..].to_vec()));
        }
    }

    async fn call(&mut self, t: u8, body: impl FnOnce(&mut Writer)) -> Result<(u8, Vec<u8>), SshError> {
        let id = self.send(t, body).await?;
        self.reply(id).await
    }

    /// A request answered by a plain STATUS.
    async fn call_status(&mut self, t: u8, body: impl FnOnce(&mut Writer)) -> Result<(), SshError> {
        match self.call(t, body).await? {
            (FXP_STATUS, b) if Reader::new(&b).u32()? == 0 => Ok(()),
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    async fn call_attrs(&mut self, t: u8, body: impl FnOnce(&mut Writer)) -> Result<Metadata, SshError> {
        match self.call(t, body).await? {
            (FXP_ATTRS, b) => Metadata::read(&mut Reader::new(&b)),
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    /// A NAME reply with exactly one name.
    async fn call_name(&mut self, t: u8, body: impl FnOnce(&mut Writer)) -> Result<String, SshError> {
        match self.call(t, body).await? {
            (FXP_NAME, b) => {
                let mut r = Reader::new(&b);
                if r.u32()? == 0 {
                    return Err(SshError::Protocol("an empty NAME reply".into()));
                }
                r.utf8()
            }
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    async fn extended(&mut self, name: &str, body: impl FnOnce(&mut Writer)) -> Result<(u8, Vec<u8>), SshError> {
        if !self.has_extension(name) {
            return Err(SshError::Sftp { status: SftpStatus::Unsupported, message: format!("the server does not offer {name}") });
        }
        self.call(FXP_EXTENDED, |w| {
            w.string(name);
            body(w);
        })
        .await
    }

    // ── paths ────────────────────────────────────────────────────────────

    /// The absolute, canonical form of `path` (`.` is the login directory).
    pub async fn canonicalize(&mut self, path: &str) -> Result<String, SshError> {
        self.call_name(FXP_REALPATH, |w| {
            w.string(path);
        })
        .await
    }

    /// A path's attributes, following symbolic links.
    pub async fn metadata(&mut self, path: &str) -> Result<Metadata, SshError> {
        self.call_attrs(FXP_STAT, |w| {
            w.string(path);
        })
        .await
    }

    /// A path's attributes; a symbolic link is described, not followed.
    pub async fn symlink_metadata(&mut self, path: &str) -> Result<Metadata, SshError> {
        self.call_attrs(FXP_LSTAT, |w| {
            w.string(path);
        })
        .await
    }

    /// Whether `path` exists (following links).
    pub async fn exists(&mut self, path: &str) -> Result<bool, SshError> {
        match self.metadata(path).await {
            Ok(_) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Change attributes: the fields that are `Some` (size truncates or extends;
    /// permissions; owner; access and modification times together).
    pub async fn set_metadata(&mut self, path: &str, m: &Metadata) -> Result<(), SshError> {
        self.call_status(FXP_SETSTAT, |w| {
            w.string(path);
            m.write(w);
        })
        .await
    }

    /// Set the permission bits.
    pub async fn set_permissions(&mut self, path: &str, mode: u32) -> Result<(), SshError> {
        self.set_metadata(path, &Metadata { permissions: Some(mode), ..Metadata::default() }).await
    }

    /// Set the access and modification times (seconds since the epoch).
    pub async fn set_times(&mut self, path: &str, atime: u32, mtime: u32) -> Result<(), SshError> {
        self.set_metadata(path, &Metadata { atime: Some(atime), mtime: Some(mtime), ..Metadata::default() }).await
    }

    /// The entries of a directory (without `.` and `..`), in the server's order.
    pub async fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, SshError> {
        let handle = match self
            .call(FXP_OPENDIR, |w| {
                w.string(path);
            })
            .await?
        {
            (FXP_HANDLE, b) => Reader::new(&b).string()?.to_vec(),
            (FXP_STATUS, b) => return Err(status_error(&b)),
            (t, _) => return Err(unexpected(t)),
        };
        let mut out = Vec::new();
        let result = loop {
            match self
                .call(FXP_READDIR, |w| {
                    w.string(&handle);
                })
                .await
            {
                Ok((FXP_NAME, b)) => {
                    let mut r = Reader::new(&b);
                    for _ in 0..r.u32()? {
                        let name = r.utf8()?;
                        let long_name = r.utf8()?;
                        let metadata = Metadata::read(&mut r)?;
                        if name != "." && name != ".." {
                            out.push(DirEntry { name, metadata, long_name });
                        }
                    }
                }
                Ok((FXP_STATUS, b)) if Reader::new(&b).u32()? == 1 => break Ok(()),
                Ok((FXP_STATUS, b)) => break Err(status_error(&b)),
                Ok((t, _)) => break Err(unexpected(t)),
                Err(e) => break Err(e),
            }
        };
        let closed = self.close_handle(&handle).await;
        result?;
        closed?;
        Ok(out)
    }

    /// Make a directory (permissions 0o755 before the server's umask).
    pub async fn create_dir(&mut self, path: &str) -> Result<(), SshError> {
        self.call_status(FXP_MKDIR, |w| {
            w.string(path);
            Metadata { permissions: Some(0o755), ..Metadata::default() }.write(w);
        })
        .await
    }

    /// Make a directory and any missing parents (an existing one is fine).
    pub async fn create_dir_all(&mut self, path: &str) -> Result<(), SshError> {
        let mut built = String::new();
        for (i, part) in path.split('/').enumerate() {
            if i > 0 || path.starts_with('/') {
                built.push('/');
            }
            built.push_str(part);
            if part.is_empty() || part == "." {
                continue;
            }
            match self.metadata(&built).await {
                Ok(m) if m.is_dir() => continue,
                Ok(_) => return Err(SshError::Sftp { status: SftpStatus::Failure, message: format!("{built} exists and is not a directory") }),
                Err(e) if e.is_not_found() => self.create_dir(&built).await?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Remove an empty directory.
    pub async fn remove_dir(&mut self, path: &str) -> Result<(), SshError> {
        self.call_status(FXP_RMDIR, |w| {
            w.string(path);
        })
        .await
    }

    /// Remove a file (or a symbolic link — not what it points to).
    pub async fn remove_file(&mut self, path: &str) -> Result<(), SshError> {
        self.call_status(FXP_REMOVE, |w| {
            w.string(path);
        })
        .await
    }

    /// Rename, replacing `to` if it exists (POSIX `rename`, through
    /// `posix-rename@openssh.com`; plain SFTP rename refuses an existing target).
    pub async fn rename(&mut self, from: &str, to: &str) -> Result<(), SshError> {
        if self.has_extension("posix-rename@openssh.com") {
            return match self
                .extended("posix-rename@openssh.com", |w| {
                    w.string(from).string(to);
                })
                .await?
            {
                (FXP_STATUS, b) if Reader::new(&b).u32()? == 0 => Ok(()),
                (FXP_STATUS, b) => Err(status_error(&b)),
                (t, _) => Err(unexpected(t)),
            };
        }
        self.call_status(FXP_RENAME, |w| {
            w.string(from).string(to);
        })
        .await
    }

    /// Make a symbolic link at `link` pointing to `target`.
    pub async fn symlink(&mut self, target: &str, link: &str) -> Result<(), SshError> {
        // OpenSSH has always taken the two the other way round from the draft.
        let (first, second) = if self.openssh { (target, link) } else { (link, target) };
        self.call_status(FXP_SYMLINK, |w| {
            w.string(first).string(second);
        })
        .await
    }

    /// Where a symbolic link points.
    pub async fn read_link(&mut self, path: &str) -> Result<String, SshError> {
        self.call_name(FXP_READLINK, |w| {
            w.string(path);
        })
        .await
    }

    /// Make a hard link (`hardlink@openssh.com`).
    pub async fn hard_link(&mut self, existing: &str, link: &str) -> Result<(), SshError> {
        match self
            .extended("hardlink@openssh.com", |w| {
                w.string(existing).string(link);
            })
            .await?
        {
            (FXP_STATUS, b) if Reader::new(&b).u32()? == 0 => Ok(()),
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    /// The file system holding `path` (`statvfs@openssh.com`).
    pub async fn fs_stats(&mut self, path: &str) -> Result<FsStats, SshError> {
        match self
            .extended("statvfs@openssh.com", |w| {
                w.string(path);
            })
            .await?
        {
            (FXP_EXTENDED_REPLY, b) => {
                let mut r = Reader::new(&b);
                let _bsize = r.u64()?;
                let frsize = r.u64()?;
                let (blocks, bfree, bavail) = (r.u64()?, r.u64()?, r.u64()?);
                for _ in 0..5 {
                    r.u64()?;
                }
                let name_max = r.u64()?;
                Ok(FsStats { block_size: frsize, blocks, blocks_free: bfree, blocks_available: bavail, name_max })
            }
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    // ── files ────────────────────────────────────────────────────────────

    /// Open a file.
    pub async fn open(&mut self, path: &str, options: OpenOptions) -> Result<File, SshError> {
        let flags = options.flags();
        match self
            .call(FXP_OPEN, |w| {
                w.string(path).u32(flags);
                let perms = if flags & FXF_CREAT != 0 { Some(0o644) } else { None };
                Metadata { permissions: perms, ..Metadata::default() }.write(w);
            })
            .await?
        {
            (FXP_HANDLE, b) => Ok(File { handle: Reader::new(&b).string()?.to_vec() }),
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    async fn close_handle(&mut self, handle: &[u8]) -> Result<(), SshError> {
        self.call_status(FXP_CLOSE, |w| {
            w.string(handle);
        })
        .await
    }

    /// Close a file (the server flushes it).
    pub async fn close(&mut self, file: File) -> Result<(), SshError> {
        self.close_handle(&file.handle).await
    }

    /// An open file's attributes.
    pub async fn file_metadata(&mut self, file: &File) -> Result<Metadata, SshError> {
        let h = file.handle.clone();
        self.call_attrs(FXP_FSTAT, |w| {
            w.string(&h);
        })
        .await
    }

    /// Flush an open file to stable storage (`fsync@openssh.com`).
    pub async fn sync(&mut self, file: &File) -> Result<(), SshError> {
        let h = file.handle.clone();
        match self
            .extended("fsync@openssh.com", |w| {
                w.string(&h);
            })
            .await?
        {
            (FXP_STATUS, b) if Reader::new(&b).u32()? == 0 => Ok(()),
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    /// Up to `len` bytes at `offset`; empty at the end of the file. The server may
    /// return fewer bytes than asked before the end.
    pub async fn read_at(&mut self, file: &File, offset: u64, len: u32) -> Result<Vec<u8>, SshError> {
        let h = file.handle.clone();
        match self
            .call(FXP_READ, |w| {
                w.string(&h).u64(offset).u32(len);
            })
            .await?
        {
            (FXP_DATA, b) => Ok(Reader::new(&b).string()?.to_vec()),
            (FXP_STATUS, b) if Reader::new(&b).u32()? == 1 => Ok(Vec::new()),
            (FXP_STATUS, b) => Err(status_error(&b)),
            (t, _) => Err(unexpected(t)),
        }
    }

    /// Write `data` at `offset`, in pipelined requests.
    pub async fn write_at(&mut self, file: &File, offset: u64, data: &[u8]) -> Result<(), SshError> {
        let h = file.handle.clone();
        let mut pending = Vec::new();
        let mut first_error = None;
        let mut pos = 0usize;
        while pos < data.len() || !pending.is_empty() {
            while pos < data.len() && pending.len() < IN_FLIGHT && first_error.is_none() {
                let n = (data.len() - pos).min(CHUNK as usize);
                let at = offset + pos as u64;
                let chunk = &data[pos..pos + n];
                let id = self
                    .send(FXP_WRITE, |w| {
                        w.string(&h).u64(at).string(chunk);
                    })
                    .await?;
                pending.push(id);
                pos += n;
            }
            if pending.is_empty() {
                break;
            }
            let (id, t, b) = self.reply_any().await?;
            let Some(i) = pending.iter().position(|&p| p == id) else {
                self.stash.insert(id, (t, b));
                continue;
            };
            pending.swap_remove(i);
            match t {
                FXP_STATUS if Reader::new(&b).u32()? == 0 => {}
                FXP_STATUS => {
                    first_error.get_or_insert_with(|| status_error(&b));
                }
                t => {
                    first_error.get_or_insert_with(|| unexpected(t));
                }
            }
            if first_error.is_some() && pos < data.len() {
                pos = data.len(); // stop sending; drain what is in flight
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Read a whole file, with many requests in flight.
    pub async fn read(&mut self, path: &str) -> Result<Vec<u8>, SshError> {
        let file = self.open(path, OpenOptions { read: true, ..OpenOptions::default() }).await?;
        let result = self.read_all(&file).await;
        let closed = self.close(file).await;
        let data = result?;
        closed?;
        Ok(data)
    }

    async fn read_all(&mut self, file: &File) -> Result<Vec<u8>, SshError> {
        let h = file.handle.clone();
        let mut next = 0u64;
        let mut eof: Option<u64> = None;
        let mut inflight: HashMap<u32, (u64, u32)> = HashMap::new();
        let mut parts: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut failure = None;
        loop {
            while inflight.len() < IN_FLIGHT && eof.is_none() && failure.is_none() {
                let at = next;
                let id = self
                    .send(FXP_READ, |w| {
                        w.string(&h).u64(at).u32(CHUNK);
                    })
                    .await?;
                inflight.insert(id, (at, CHUNK));
                next += u64::from(CHUNK);
            }
            if inflight.is_empty() {
                break;
            }
            let (id, t, b) = self.reply_any().await?;
            let Some((at, len)) = inflight.remove(&id) else {
                self.stash.insert(id, (t, b));
                continue;
            };
            match t {
                FXP_DATA => {
                    let d = Reader::new(&b).string()?.to_vec();
                    let got = d.len() as u32;
                    if got == 0 {
                        eof = Some(eof.map_or(at, |e| e.min(at)));
                    } else {
                        if got < len {
                            // A short read: ask for the rest of this range.
                            let (rest_at, rest_len) = (at + u64::from(got), len - got);
                            let id = self
                                .send(FXP_READ, |w| {
                                    w.string(&h).u64(rest_at).u32(rest_len);
                                })
                                .await?;
                            inflight.insert(id, (rest_at, rest_len));
                        }
                        parts.insert(at, d);
                    }
                }
                FXP_STATUS if Reader::new(&b).u32()? == 1 => eof = Some(eof.map_or(at, |e| e.min(at))),
                FXP_STATUS => {
                    failure.get_or_insert_with(|| status_error(&b));
                }
                t => {
                    failure.get_or_insert_with(|| unexpected(t));
                }
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        let end = eof.unwrap_or(next);
        let mut out = Vec::new();
        for (at, d) in parts {
            if at >= end {
                break;
            }
            if at != out.len() as u64 {
                return Err(SshError::Protocol(format!("a gap in the file at {} (the server's reads did not line up)", out.len())));
            }
            out.extend(d);
        }
        Ok(out)
    }

    /// Write a whole file: created if missing, replaced if not.
    pub async fn write(&mut self, path: &str, data: &[u8]) -> Result<(), SshError> {
        let file = self.open(path, OpenOptions { write: true, create: true, truncate: true, ..OpenOptions::default() }).await?;
        let result = self.write_at(&file, 0, data).await;
        let closed = self.close(file).await;
        result?;
        closed
    }

    /// Add `data` to the end of a file (created if missing).
    pub async fn append(&mut self, path: &str, data: &[u8]) -> Result<(), SshError> {
        let file = self.open(path, OpenOptions { write: true, create: true, append: true, ..OpenOptions::default() }).await?;
        let result = async {
            // Servers differ on whether APPEND moves writes to the end: write there.
            let size = self.file_metadata(&file).await?.size.unwrap_or(0);
            self.write_at(&file, size, data).await
        }
        .await;
        let closed = self.close(file).await;
        result?;
        closed
    }
}
