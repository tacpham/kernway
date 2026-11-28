# Remote server management with kernway-ssh

*Tóm tắt: hướng dẫn dựng công cụ quản lý server từ xa (kiểu Bitvise) trên
`kernway-ssh`. Một lần đăng nhập dùng chung cho nhiều terminal, trình quản lý
file SFTP, lệnh chạy nền và port forwarding cùng lúc.*

`kernway-ssh` is a self-written SSH 2 client: fully async on the kernway reactor,
on Linux, macOS and Windows. This page is the map for building a remote server
manager on it: terminals, a file browser, commands and tunnels over one login.
What it does not do yet is in [`GAPS.md`](../GAPS.md#ssh); what is proven against
real servers is in [`acceptance/ssh.md`](../acceptance/ssh.md).

## 1. Log in

```rust
use kernway_ssh::*;

let key = std::fs::read(key_path)?;                       // read on the blocking pool in real code
let key = if PrivateKey::is_encrypted(&key) {
    PrivateKey::from_pem_with_passphrase(&key, &passphrase)? // ask the user first
} else {
    PrivateKey::from_pem(&key)?
};
let config = SshConfig::new("server.example", "deploy")
    .port(22)
    .auth(Auth::Key(key))                                   // tried in order:
    .auth(Auth::Agent(Agent::from_env().unwrap()))          // ssh-agent keys
    .auth(Auth::Password(password))                         // password / keyboard-interactive
    .host_key(HostKeyPolicy::KnownHosts(known_hosts_path)); // ~/.ssh/known_hosts by default
let session = SshClient::connect(config).await?.into_session();
```

- **Host keys.** An unknown host fails with `SshError::HostKey`, and the message
  names its `SHA256:` fingerprint. A UI shows it, asks the user, then reconnects
  with `HostKeyPolicy::Fingerprint(fp)` and appends a line to `known_hosts`:
  `format!("[{host}]:{port} {}", session.host_key().to_openssh())`, or just
  `{host}` in place of `[{host}]:{port}` on port 22. A *changed* key says `CHANGED`: treat it as
  a possible attack, never as a prompt.
- **Refused logins** return `SshError::Auth { allowed }`, the methods the server
  still accepts. Use them to decide what to ask for next.
- `Agent::from_env()` uses `SSH_AUTH_SOCK`, or the Windows OpenSSH agent pipe.

## 2. Many things at once on one login

`SshSession` is `Clone`; hand a clone to each panel or task. One task on the shard
drives the connection. Every channel has its own queue, so a slow panel never
stalls another.

| Need | Call |
|---|---|
| A terminal tab | `session.console(PtyConfig { term, cols, rows })` → `Console` |
| A command, output as it comes | `session.console_exec(cmd, None)` |
| A command, collected | `session.exec(cmd)` → `ExecOutput { stdout, stderr, exit_status, exit_signal }` |
| A file browser | `session.sftp()` → `Sftp` (open one per panel / transfer) |
| A tunnel | `forward_local`, `forward_dynamic` (SOCKS5), `forward_remote` |

OpenSSH allows **10 channels per connection** by default (`MaxSessions`): terminals,
SFTP sessions, running commands and open tunnel connections all count. Past it,
opening fails with `SshError::Channel`; open a second login for more.

## 3. A terminal tab

```rust
let mut tab = session.console(PtyConfig { term: "xterm-256color".into(), cols: 120, rows: 32 }).await?;
tab.write(keystrokes)?;           // bytes from xterm.js / the keyboard (Ctrl-C is [3])
tab.resize(cols, rows)?;          // on window resize
while let Some(event) = tab.next().await {
    match event {
        ConsoleEvent::Output(bytes) => render(bytes),       // raw terminal bytes, escape codes included
        ConsoleEvent::Exit { status, .. } => show_exit(status),
        ConsoleEvent::Closed(why) => break,                 // why = Some(..) when the connection failed
        ConsoleEvent::Stderr(_) => {}                       // only without a pty
    }
}
```

For a browser terminal, bridge a WebSocket to `write` and `next`. The bytes are the
pty's, so feed them to xterm.js unchanged.

## 4. The file manager (SFTP)

```rust
let mut fs = session.sftp().await?;
let home = fs.canonicalize(".").await?;
for entry in fs.read_dir(&home).await? {                     // name, metadata (size, mode, times, kind), long_name
    println!("{} {} {:?}", entry.long_name, entry.name, entry.metadata.size);
}
fs.download_to("/var/log/app.log", local_path, |done, total| progress(done, total)).await?;
fs.upload_from(local_path, "/srv/app/config.toml", |done, total| progress(done, total)).await?;
fs.upload_dir(local_dir, "/srv/app/static").await?;          // trees both ways
fs.download_dir("/etc/nginx", local_dir).await?;
fs.rename("a.txt", "b.txt").await?;                          // replaces b.txt (posix-rename)
fs.create_dir_all("x/y/z").await?;
fs.remove_dir_all("old-release").await?;                     // links removed, not followed
fs.set_permissions("deploy.sh", 0o755).await?;
fs.symlink("releases/42", "current").await?;
let space = fs.fs_stats(".").await?.available_bytes();
let edit = fs.read("small-config.yml").await?;               // whole small files in memory
fs.write("small-config.yml", &edited).await?;
```

- Transfers are streamed, with many requests in flight and memory bounded. To
  cancel, drop the future. Run a transfer in its own task on its own `Sftp`, and
  the browsing panel stays responsive, because SFTP requests on one `Sftp` go one
  after another.
- Errors are `SshError::Sftp { status, message }`. Use `e.is_not_found()`, or
  `status == SftpStatus::PermissionDenied`, to show the right message.
- `Metadata::{is_dir, is_file, is_symlink, mode}` give the icon and the permission
  column; `read_link` gives a link's target.
- When the server has no SFTP subsystem, SCP still works:
  `SshClient::scp_upload`, `scp_download` and `*_dir`, over a connection of its own.

## 5. Tunnels

```rust
let db = session.forward_local("127.0.0.1:15432".parse()?, "127.0.0.1", 5432).await?; // ssh -L
let socks = session.forward_dynamic("127.0.0.1:1080".parse()?).await?;                 // ssh -D
let back = session.forward_remote("127.0.0.1", 0, "127.0.0.1", 8080).await?;           // ssh -R
println!("server port {}", back.port());
db.stop(); // or drop it
```

The server must allow forwarding (`AllowTcpForwarding yes`); a refusal names that.

## 6. Ending

`session.disconnect()` ends everything on the login. Dropping every handle ends it
too, except a remote forward, which keeps it up until `disconnect()`.
`session.is_closed()` tells a UI that the connection is gone, and each open
console or channel then receives `Closed(Some(reason))`.
