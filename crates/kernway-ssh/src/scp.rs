//! SCP: the rcp protocol over `scp -t` (to the server) and `scp -f` (from it), the
//! way OpenSSH's `scp -O` speaks it. Every server with an `scp` binary answers it —
//! including those with no SFTP subsystem. Files keep their mode and, with times,
//! their modification and access times; directories copy recursively.
//!
//! Local files are read and written on the blocking pool; a file is held in memory
//! while it is copied.

use crate::client::SshClient;
use crate::SshError;
use std::path::{Path, PathBuf};

/// A file copied from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScpFile {
    /// The file name the server sent.
    pub name: String,
    /// The permission bits (`0o644`).
    pub mode: u32,
    /// Modification and access times (seconds since the epoch), when sent.
    pub times: Option<(u64, u64)>,
    /// The contents.
    pub data: Vec<u8>,
}

/// Quote `s` for the remote POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn scp_err(m: impl Into<String>) -> SshError {
    SshError::Channel(format!("scp: {}", m.into()))
}

/// The byte stream of one scp session, with the buffering its line protocol needs.
struct Session<'a> {
    ssh: &'a mut SshClient,
    id: u32,
    buf: Vec<u8>,
}

impl Session<'_> {
    async fn more(&mut self) -> Result<(), SshError> {
        let d = self.ssh.read(self.id, usize::MAX).await?;
        if d.is_empty() {
            let c = self.ssh.chan(self.id)?;
            let err = String::from_utf8_lossy(&std::mem::take(&mut c.stderr)).trim().to_string();
            return Err(scp_err(if err.is_empty() { "the remote scp ended early".to_owned() } else { err }));
        }
        self.buf.extend(d);
        Ok(())
    }

    async fn byte(&mut self) -> Result<u8, SshError> {
        if self.buf.is_empty() {
            self.more().await?;
        }
        Ok(self.buf.remove(0))
    }

    async fn line(&mut self) -> Result<String, SshError> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let l: Vec<u8> = self.buf.drain(..=i).collect();
                return Ok(String::from_utf8_lossy(&l[..l.len() - 1]).into_owned());
            }
            if self.buf.len() > 64 * 1024 {
                return Err(scp_err("an over-long protocol line"));
            }
            self.more().await?;
        }
    }

    async fn exact(&mut self, n: usize) -> Result<Vec<u8>, SshError> {
        while self.buf.len() < n {
            self.more().await?;
        }
        Ok(self.buf.drain(..n).collect())
    }

    /// The remote's answer: 0 fine, 1 a warning, 2 a fatal error (both with a message).
    async fn ack(&mut self) -> Result<(), SshError> {
        match self.byte().await? {
            0 => Ok(()),
            1 | 2 => Err(scp_err(self.line().await?)),
            b => Err(scp_err(format!("an unexpected reply byte {b}"))),
        }
    }

    async fn send(&mut self, data: &[u8]) -> Result<(), SshError> {
        self.ssh.write(self.id, data).await
    }

    async fn finish(self) -> Result<(), SshError> {
        self.ssh.send_eof(self.id).await?;
        self.ssh.wait_closed(self.id).await?;
        let c = self.ssh.chan(self.id)?;
        let status = c.exit_status;
        let stderr = String::from_utf8_lossy(&c.stderr).trim().to_string();
        self.ssh.close(self.id).await?;
        match status {
            Some(0) | None => Ok(()),
            Some(s) => Err(scp_err(format!("the remote scp exited with {s}: {stderr}"))),
        }
    }
}

/// One thing to send: a file, or a directory with its entries.
enum Item {
    File { name: String, mode: u32, times: Option<(u64, u64)>, data: Vec<u8> },
    Dir { name: String, mode: u32, times: Option<(u64, u64)>, entries: Vec<Item> },
}

impl SshClient {
    async fn scp_open(&mut self, command: &str) -> Result<Session<'_>, SshError> {
        let id = self.open_session().await?;
        let ok = self
            .request(id, "exec", |w| {
                w.string(command);
            })
            .await?;
        if !ok {
            return Err(scp_err("the server refused to run scp"));
        }
        Ok(Session { ssh: self, id, buf: Vec::new() })
    }

    /// Copy `data` to the server as `remote` (a file path, or an existing directory
    /// to put a file named after it in), with permissions `mode` and, when given,
    /// `(mtime, atime)`.
    pub async fn scp_upload(&mut self, remote: &str, data: &[u8], mode: u32, times: Option<(u64, u64)>) -> Result<(), SshError> {
        let name = remote.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("file").to_owned();
        let item = Item::File { name, mode, times, data: data.to_vec() };
        self.scp_send(remote, &item, false).await
    }

    /// Copy the local directory `local` (recursively, with modes and times) to the
    /// server as `remote`.
    pub async fn scp_upload_dir(&mut self, local: &Path, remote: &str) -> Result<(), SshError> {
        let local = local.to_path_buf();
        let item = rt_core::spawn_blocking(move || read_tree_blocking(&local)).await.ok_or_else(|| scp_err("the blocking pool is gone"))??;
        self.scp_send(remote, &item, true).await
    }

    async fn scp_send(&mut self, remote: &str, item: &Item, recursive: bool) -> Result<(), SshError> {
        let flags = if recursive { "-r -p" } else { "-p" };
        let mut s = self.scp_open(&format!("scp {flags} -t {}", shell_quote(remote))).await?;
        s.ack().await?;
        send_item(&mut s, item).await?;
        s.finish().await
    }

    /// Copy the file `remote` from the server.
    pub async fn scp_download(&mut self, remote: &str) -> Result<ScpFile, SshError> {
        let mut s = self.scp_open(&format!("scp -p -f {}", shell_quote(remote))).await?;
        s.send(&[0]).await?;
        let mut times = None;
        let file = loop {
            let kind = s.byte().await?;
            let line = s.line().await?;
            match kind {
                b'T' => {
                    times = Some(parse_times(&line)?);
                    s.send(&[0]).await?;
                }
                b'C' => {
                    let (mode, size, name) = parse_entry(&line)?;
                    s.send(&[0]).await?;
                    let data = s.exact(size).await?;
                    s.ack().await?;
                    s.send(&[0]).await?;
                    break ScpFile { name, mode, times, data };
                }
                b'D' => return Err(scp_err(format!("{remote} is a directory (use scp_download_dir)"))),
                1 | 2 => return Err(scp_err(line)),
                k => return Err(scp_err(format!("an unexpected record {:?}", k as char))),
            }
        };
        s.finish().await?;
        Ok(file)
    }

    /// Copy the directory `remote` from the server, recursively, into `local` (which
    /// is created; modes and times kept). Returns the files written.
    pub async fn scp_download_dir(&mut self, remote: &str, local: &Path) -> Result<Vec<PathBuf>, SshError> {
        let mut s = self.scp_open(&format!("scp -r -p -f {}", shell_quote(remote))).await?;
        s.send(&[0]).await?;
        let mut stack: Vec<PathBuf> = Vec::new();
        let mut written = Vec::new();
        let mut times = None;
        let mut dir_times: Vec<(PathBuf, Option<(u64, u64)>)> = Vec::new();
        loop {
            let kind = match s.byte().await {
                Ok(k) => k,
                Err(e) => return Err(e),
            };
            let line = s.line().await?;
            match kind {
                b'T' => times = Some(parse_times(&line)?),
                b'D' => {
                    let (mode, _, name) = parse_entry(&line)?;
                    check_name(&name)?;
                    let path = match stack.last() {
                        Some(p) => p.join(&name),
                        None => local.to_path_buf(),
                    };
                    let p2 = path.clone();
                    pool(rt_core::spawn_blocking(move || {
                        std::fs::create_dir_all(&p2)?;
                        set_mode_blocking(&p2, mode)
                    })
                    .await)?;
                    dir_times.push((path.clone(), times.take()));
                    stack.push(path);
                }
                b'E' => {
                    let (path, t) = dir_times.pop().ok_or_else(|| scp_err("an E record with no open directory"))?;
                    stack.pop();
                    if let Some(t) = t {
                        // Best effort: not every platform lets a directory's times be set.
                        let _ = rt_core::spawn_blocking(move || set_times_blocking(&path, t)).await;
                    }
                    if stack.is_empty() {
                        s.send(&[0]).await?;
                        break;
                    }
                }
                b'C' => {
                    let (mode, size, name) = parse_entry(&line)?;
                    check_name(&name)?;
                    s.send(&[0]).await?;
                    let data = s.exact(size).await?;
                    s.ack().await?;
                    let dir = stack.last().cloned().ok_or_else(|| scp_err(format!("{remote} is a file (use scp_download)")))?;
                    let path = dir.join(&name);
                    let p2 = path.clone();
                    let t = times.take();
                    pool(rt_core::spawn_blocking(move || {
                        std::fs::write(&p2, &data)?;
                        set_mode_blocking(&p2, mode)?;
                        if let Some(t) = t {
                            set_times_blocking(&p2, t)?;
                        }
                        Ok(())
                    })
                    .await)?;
                    written.push(path);
                }
                1 | 2 => return Err(scp_err(line)),
                k => return Err(scp_err(format!("an unexpected record {:?}", k as char))),
            }
            s.send(&[0]).await?;
        }
        s.finish().await?;
        Ok(written)
    }
}

fn send_item<'a>(s: &'a mut Session<'_>, item: &'a Item) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SshError>> + 'a>> {
    Box::pin(async move {
        let times = match item {
            Item::File { times, .. } | Item::Dir { times, .. } => *times,
        };
        if let Some((m, a)) = times {
            s.send(format!("T{m} 0 {a} 0\n").as_bytes()).await?;
            s.ack().await?;
        }
        match item {
            Item::File { name, mode, data, .. } => {
                check_name(name)?;
                s.send(format!("C{:04o} {} {name}\n", mode & 0o7777, data.len()).as_bytes()).await?;
                s.ack().await?;
                s.send(data).await?;
                s.send(&[0]).await?;
                s.ack().await
            }
            Item::Dir { name, mode, entries, .. } => {
                check_name(name)?;
                s.send(format!("D{:04o} 0 {name}\n", mode & 0o7777).as_bytes()).await?;
                s.ack().await?;
                for e in entries {
                    send_item(s, e).await?;
                }
                s.send(b"E\n").await?;
                s.ack().await
            }
        }
    })
}

/// A name in an scp record must be one path element (a hostile server could
/// otherwise write outside the target with `../` or `/`).
fn check_name(name: &str) -> Result<(), SshError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\n') || name.contains('\0') {
        return Err(scp_err(format!("a refused file name {name:?}")));
    }
    Ok(())
}

/// `T<mtime> 0 <atime> 0`.
fn parse_times(line: &str) -> Result<(u64, u64), SshError> {
    let p: Vec<&str> = line.split(' ').collect();
    match p.as_slice() {
        [m, _, a, _] => Ok((m.parse().map_err(|_| scp_err("a bad T record"))?, a.parse().map_err(|_| scp_err("a bad T record"))?)),
        _ => Err(scp_err("a bad T record")),
    }
}

/// `C0644 <size> <name>` / `D0755 0 <name>` (the kind byte already taken).
fn parse_entry(line: &str) -> Result<(u32, usize, String), SshError> {
    let mut p = line.splitn(3, ' ');
    let (Some(mode), Some(size), Some(name)) = (p.next(), p.next(), p.next()) else {
        return Err(scp_err(format!("a bad record {line:?}")));
    };
    let mode = u32::from_str_radix(mode, 8).map_err(|_| scp_err("a bad mode"))?;
    let size = size.parse().map_err(|_| scp_err("a bad size"))?;
    Ok((mode, size, name.to_owned()))
}

/// The outcome of file work done on the blocking pool.
fn pool<T>(r: Option<std::io::Result<T>>) -> Result<T, SshError> {
    r.ok_or_else(|| scp_err("the blocking pool is gone"))?.map_err(SshError::Io)
}

// The `*_blocking` functions touch the local file system: they run only on the
// blocking pool (inside `spawn_blocking`), which the source scan checks.

fn read_tree_blocking(path: &Path) -> Result<Item, SshError> {
    let meta = std::fs::metadata(path)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "dir".into());
    let times = file_times_blocking(&meta);
    let mode = file_mode_blocking(&meta);
    if meta.is_dir() {
        let mut entries = Vec::new();
        let mut list: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
        list.sort_by_key(|e| e.file_name());
        for e in list {
            entries.push(read_tree_blocking(&e.path())?);
        }
        Ok(Item::Dir { name, mode, times, entries })
    } else {
        Ok(Item::File { name, mode, times, data: std::fs::read(path)? })
    }
}

fn file_times_blocking(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    let secs = |t: std::io::Result<std::time::SystemTime>| t.ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs());
    Some((secs(meta.modified())?, secs(meta.accessed()).unwrap_or(0)))
}

#[cfg(unix)]
fn file_mode_blocking(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode_blocking(meta: &std::fs::Metadata) -> u32 {
    match (meta.is_dir(), meta.permissions().readonly()) {
        (true, _) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    }
}

#[cfg(unix)]
fn set_mode_blocking(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
}

#[cfg(not(unix))]
fn set_mode_blocking(path: &Path, mode: u32) -> std::io::Result<()> {
    let mut p = std::fs::metadata(path)?.permissions();
    p.set_readonly(mode & 0o200 == 0);
    std::fs::set_permissions(path, p)
}

fn set_times_blocking(path: &Path, (mtime, atime): (u64, u64)) -> std::io::Result<()> {
    let at = |s: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s);
    let f = std::fs::File::options().write(true).open(path).or_else(|_| std::fs::File::open(path))?;
    f.set_times(std::fs::FileTimes::new().set_modified(at(mtime)).set_accessed(at(atime)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_parse_and_hostile_names_are_refused() {
        assert_eq!(parse_entry("0644 12 notes.txt").unwrap(), (0o644, 12, "notes.txt".into()));
        assert_eq!(parse_entry("0755 0 a dir with spaces").unwrap(), (0o755, 0, "a dir with spaces".into()));
        assert_eq!(parse_times("1234567890 0 1000000000 0").unwrap(), (1_234_567_890, 1_000_000_000));
        assert!(parse_entry("rwx 1 x").is_err());
        for bad in ["..", ".", "", "a/b", "/etc/passwd", "x\ny"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(shell_quote("it's here"), r"'it'\''s here'");
    }
}
