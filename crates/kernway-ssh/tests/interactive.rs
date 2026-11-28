//! Key logins (passphrase-protected keys, an ssh-agent), interactive consoles and
//! SCP against a real OpenSSH server — set up by `tests/support/ssh-setup.sh`.
//! Without it each test reports `skip:`, which CI and the acceptance run treat as
//! a failure.

use kernway_ssh::*;
use std::time::{Duration, Instant};

struct Live {
    host: String,
    port: u16,
    user: String,
    dir: String,
    agent: String,
}

fn live() -> Option<Live> {
    let Ok(host) = std::env::var("KERNWAY_SSH_HOST") else {
        eprintln!("skip: KERNWAY_SSH_HOST not set (tests/support/ssh-setup.sh)");
        return None;
    };
    let v = |k: &str| std::env::var(k).unwrap();
    Some(Live { host, port: v("KERNWAY_SSH_PORT").parse().unwrap(), user: v("KERNWAY_SSH_USER"), dir: v("KERNWAY_SSH_DIR"), agent: v("KERNWAY_SSH_AGENT_SOCK") })
}

impl Live {
    fn file(&self, name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/keys/{name}", self.dir)).unwrap()
    }

    fn base(&self) -> SshConfig {
        SshConfig::new(&self.host, &self.user).port(self.port).host_key(HostKeyPolicy::KnownHosts(format!("{}/known_hosts", self.dir).into()))
    }

    async fn connect(&self) -> SshClient {
        SshClient::connect(self.base().auth(Auth::Key(PrivateKey::from_pem(&self.file("id_ed25519")).unwrap()))).await.unwrap()
    }
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn scratch(name: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("kwi-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

fn bytes(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(7);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

#[test]
fn passphrase_protected_keys_log_in() {
    let Some(l) = live() else { return };
    block(async move {
        for (name, pass) in [("id_ed25519_enc", "a passphrase"), ("id_ecdsa_enc", "mật khẩu ✓"), ("id_rsa_enc", "a passphrase"), ("id_ed25519_gcm", "a passphrase")] {
            let pem = l.file(name);
            assert!(PrivateKey::is_encrypted(&pem), "{name}");
            let e = PrivateKey::from_pem(&pem).unwrap_err();
            assert!(e.to_string().contains("passphrase"), "{name}: {e}");
            let e = PrivateKey::from_pem_with_passphrase(&pem, "not it").unwrap_err();
            assert!(e.to_string().contains("wrong passphrase"), "{name}: {e}");
            let key = PrivateKey::from_pem_with_passphrase(&pem, pass).unwrap_or_else(|e| panic!("{name}: {e}"));
            let public = std::fs::read_to_string(format!("{}/keys/{name}.pub", l.dir)).unwrap();
            assert_eq!(*key.public_key(), PublicKey::from_openssh(&public).unwrap(), "{name}: the same key as ssh-keygen's .pub");
            let mut c = SshClient::connect(l.base().auth(Auth::Key(key))).await.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(c.exec("id -un").await.unwrap().stdout_text().trim(), l.user);
        }
        assert!(!PrivateKey::is_encrypted(&l.file("id_ed25519")));
        assert!(PrivateKey::from_pem_with_passphrase(&l.file("id_ed25519"), "ignored").is_ok(), "an unprotected key ignores the passphrase");
    });
}

#[test]
fn keys_from_an_agent_and_several_keys_in_turn_log_in() {
    let Some(l) = live() else { return };
    block(async move {
        let agent = Agent::new(&l.agent);
        let ids = agent.identities().await.unwrap();
        let held = PublicKey::from_openssh(&std::fs::read_to_string(format!("{}/keys/id_agent.pub", l.dir)).unwrap()).unwrap();
        assert!(ids.iter().any(|(k, c)| *k == held && c == "kw-agent"), "{ids:?}");
        assert!(!std::path::Path::new(&format!("{}/keys/id_agent", l.dir)).exists(), "only the agent holds the private key");
        let mut c = SshClient::connect(l.base().auth(Auth::Agent(agent))).await.unwrap();
        assert_eq!(c.exec("echo via-agent").await.unwrap().stdout_text(), "via-agent\n");
        let e = Agent::new(format!("{}/no-agent.sock", l.dir)).identities().await.unwrap_err();
        assert!(e.to_string().contains("ssh-agent"), "{e}");

        // A key the server does not know first, then one it does.
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let stranger = PrivateKey::from_pem(format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", kernway_wire::base64::encode(pkcs8.as_ref())).as_bytes()).unwrap();
        let cfg = l.base().auth(Auth::Key(stranger)).auth(Auth::Key(PrivateKey::from_pem(&l.file("id_ecdsa")).unwrap()));
        assert!(SshClient::connect(cfg).await.is_ok());
    });
}

/// Read console output until `needle` shows up (or panic after `secs`).
async fn wait_for(c: &mut Console, seen: &mut String, needle: &str, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !seen.contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rt_core::timeout(left, c.next()).await {
            Ok(Some(ConsoleEvent::Output(d))) => seen.push_str(&String::from_utf8_lossy(&d)),
            Ok(Some(other)) => panic!("{other:?} while waiting for {needle:?}; so far: {seen}"),
            Ok(None) => panic!("the console ended while waiting for {needle:?}: {seen}"),
            Err(_) => panic!("no {needle:?} within {secs}s; so far: {seen}"),
        }
    }
}

#[test]
fn a_console_is_a_real_terminal_that_resizes_and_takes_ctrl_c() {
    let Some(l) = live() else { return };
    block(async move {
        let mut c = l.connect().await.console(PtyConfig::default()).await.unwrap();
        let mut seen = String::new();
        c.write("stty size; tty; echo marker-$((6*7)); printf 'xin ch\\303\\240o\\n'\n").unwrap();
        wait_for(&mut c, &mut seen, "marker-42", 10).await;
        wait_for(&mut c, &mut seen, "xin chào", 5).await;
        assert!(seen.contains("24 80"), "the terminal size: {seen}");
        assert!(seen.contains("/dev/pts/") || seen.contains("/dev/ttys"), "a pseudo-terminal (Linux / macOS): {seen}");

        c.resize(120, 40).unwrap();
        seen.clear();
        c.write("stty size\n").unwrap();
        wait_for(&mut c, &mut seen, "40 120", 10).await;

        // Ctrl-C interrupts the foreground command; the shell carries on. (Whether a
        // shell runs the rest of an interrupted `a; b` list differs — bash 3.2 does —
        // so the command stands alone, and the proof is time: sleep 30 ends at once.)
        seen.clear();
        c.write("sleep 30\n").unwrap();
        rt_core::sleep(Duration::from_millis(500)).await;
        let started = Instant::now();
        c.write([3u8]).unwrap();
        c.write("echo after-$((1+1))-int\n").unwrap();
        wait_for(&mut c, &mut seen, "after-2-int", 10).await;
        assert!(started.elapsed() < Duration::from_secs(10), "interrupted, not waited out");

        c.write("exit 3\n").unwrap();
        let mut exit = None;
        loop {
            match rt_core::timeout(Duration::from_secs(10), c.next()).await.expect("events") {
                Some(ConsoleEvent::Exit { status, .. }) => exit = Some(status),
                Some(ConsoleEvent::Closed(why)) => {
                    assert_eq!(why, None);
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        assert_eq!(exit, Some(Some(3)));
        assert!(c.write("x").is_err() || c.next().await.is_none(), "nothing after the end");
    });
}

#[test]
fn a_command_console_streams_output_takes_input_and_signals() {
    let Some(l) = live() else { return };
    block(async move {
        // Output arrives as it is written, not at the end.
        let mut c = l.connect().await.console_exec("for i in 1 2 3; do echo line$i; sleep 0.4; done; echo oops >&2", None).await.unwrap();
        let started = Instant::now();
        let (mut out, mut err, mut first_at, mut exit) = (String::new(), String::new(), None, None);
        while let Some(e) = c.next().await {
            match e {
                ConsoleEvent::Output(d) => {
                    first_at.get_or_insert(started.elapsed());
                    out.push_str(&String::from_utf8_lossy(&d));
                }
                ConsoleEvent::Stderr(d) => err.push_str(&String::from_utf8_lossy(&d)),
                ConsoleEvent::Exit { status, .. } => exit = status,
                ConsoleEvent::Closed(why) => {
                    assert_eq!(why, None);
                    break;
                }
            }
        }
        assert_eq!(out, "line1\nline2\nline3\n");
        assert_eq!(err, "oops\n", "stdout and stderr stay apart without a terminal");
        assert_eq!(exit, Some(0));
        assert!(first_at.unwrap() < Duration::from_millis(700), "the first line came before the end: {first_at:?}");

        // Standard input, byte-exact, then end of file.
        let mut c = l.connect().await.console_exec("cat", None).await.unwrap();
        let data = bytes(3_000_000, 4);
        for part in data.chunks(100_000) {
            c.write(part).unwrap();
        }
        c.send_eof().unwrap();
        let mut back = Vec::new();
        while let Some(e) = c.next().await {
            match e {
                ConsoleEvent::Output(d) => back.extend(d),
                ConsoleEvent::Closed(_) => break,
                _ => {}
            }
        }
        assert!(back == data, "3 MB through cat ({} back)", back.len());

        // A signal ends a command.
        let mut c = l.connect().await.console_exec("sleep 30", None).await.unwrap();
        rt_core::sleep(Duration::from_millis(300)).await;
        c.signal("TERM").unwrap();
        let mut signal = None;
        while let Some(e) = rt_core::timeout(Duration::from_secs(10), c.next()).await.expect("the signal ends it") {
            match e {
                ConsoleEvent::Exit { signal: s, .. } => signal = s,
                ConsoleEvent::Closed(_) => break,
                _ => {}
            }
        }
        assert_eq!(signal.as_deref(), Some("TERM"));
    });
}

#[test]
fn scp_copies_files_with_their_mode_and_times() {
    let Some(l) = live() else { return };
    block(async move {
        let mut ssh = l.connect().await;
        let mut sftp = l.connect().await.sftp().await.unwrap();
        for (i, n) in [0usize, 1, 65_536, 1_500_000].into_iter().enumerate() {
            let name = format!("{} it's-{n}.bin", scratch("scp"));
            let data = bytes(n, i as u32);
            ssh.scp_upload(&name, &data, 0o640, Some((1_234_567_890, 1_000_000_000))).await.unwrap();
            let m = sftp.metadata(&name).await.unwrap();
            assert_eq!((m.size, m.mode(), m.mtime, m.atime), (Some(n as u64), Some(0o640), Some(1_234_567_890), Some(1_000_000_000)), "{name}");
            assert!(sftp.read(&name).await.unwrap() == data);
            let back = ssh.scp_download(&name).await.unwrap();
            assert!(back.data == data, "{n} bytes back");
            // (Reading the file just now moved its access time: only mtime is still ours.)
            assert_eq!((back.mode, back.times.map(|t| t.0), back.name.as_str()), (0o640, Some(1_234_567_890), name.as_str()));
            sftp.remove_file(&name).await.unwrap();
        }
        // Into an existing directory: the file takes the name given.
        let dir = scratch("into");
        sftp.create_dir(&dir).await.unwrap();
        ssh.scp_upload(&format!("{dir}/report.csv"), b"a,b\n", 0o600, None).await.unwrap();
        assert_eq!(sftp.read(&format!("{dir}/report.csv")).await.unwrap(), b"a,b\n");
        // Errors carry the remote's message, and the connection goes on.
        let e = ssh.scp_download("kwi-does-not-exist").await.unwrap_err();
        assert!(e.to_string().contains("No such file"), "{e}");
        let e = ssh.scp_upload("/proc/kwi-forbidden", b"x", 0o644, None).await.unwrap_err();
        assert!(e.to_string().starts_with("ssh: channel: scp:"), "{e}");
        // A directory without -r: OpenSSH answers "not a regular file".
        let e = ssh.scp_download(&dir).await.unwrap_err();
        assert!(e.to_string().contains("not a regular file") || e.to_string().contains("directory"), "{e}");
        assert_eq!(ssh.exec(&format!("rm -r '{dir}' && echo ok")).await.unwrap().stdout_text(), "ok\n");
    });
}

#[test]
fn scp_copies_directory_trees_both_ways() {
    let Some(l) = live() else { return };
    block(async move {
        let tmp = std::env::temp_dir().join(scratch("tree"));
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();
        std::fs::write(src.join("a/b/c.txt"), b"deep file").unwrap();
        std::fs::write(src.join("a/x.bin"), bytes(200_000, 9)).unwrap();
        std::fs::write(src.join("top tệp.txt"), "tiếng Việt").unwrap();
        let remote = scratch("rtree");
        let mut ssh = l.connect().await;
        ssh.scp_upload_dir(&src, &remote).await.unwrap();
        let listing = ssh.exec(&format!("cd '{remote}' && find . | sort")).await.unwrap().stdout_text();
        assert_eq!(listing, ".\n./a\n./a/b\n./a/b/c.txt\n./a/x.bin\n./empty\n./top tệp.txt\n", "{listing}");

        let dst = tmp.join("dst");
        let written = ssh.scp_download_dir(&remote, &dst).await.unwrap();
        assert_eq!(written.len(), 3);
        assert_eq!(std::fs::read(dst.join("a/b/c.txt")).unwrap(), b"deep file");
        assert_eq!(std::fs::read(dst.join("a/x.bin")).unwrap(), bytes(200_000, 9));
        assert_eq!(std::fs::read_to_string(dst.join("top tệp.txt")).unwrap(), "tiếng Việt");
        assert!(dst.join("empty").is_dir());
        let e = ssh.scp_download_dir(&format!("{remote}/a/x.bin"), &tmp.join("nope")).await.unwrap_err();
        assert!(e.to_string().contains("is a file"), "{e}");
        ssh.exec(&format!("rm -r '{remote}'")).await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    });
}
