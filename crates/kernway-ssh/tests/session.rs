//! One login shared by consoles, SFTP, commands and port forwards at once — against
//! a real OpenSSH server (`tests/support/ssh-setup.sh`). Without it each test
//! reports `skip:`, which CI and the acceptance run treat as a failure.

use kernway_ssh::*;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

struct Live {
    host: String,
    port: u16,
    user: String,
    dir: String,
}

fn live() -> Option<Live> {
    let Ok(host) = std::env::var("KERNWAY_SSH_HOST") else {
        eprintln!("skip: KERNWAY_SSH_HOST not set (tests/support/ssh-setup.sh)");
        return None;
    };
    let v = |k: &str| std::env::var(k).unwrap();
    Some(Live { host, port: v("KERNWAY_SSH_PORT").parse().unwrap(), user: v("KERNWAY_SSH_USER"), dir: v("KERNWAY_SSH_DIR") })
}

impl Live {
    fn key(&self) -> PrivateKey {
        PrivateKey::from_pem(&std::fs::read(format!("{}/keys/id_ed25519", self.dir)).unwrap()).unwrap()
    }

    async fn session(&self) -> SshSession {
        let cfg = SshConfig::new(&self.host, &self.user).port(self.port).auth(Auth::Key(self.key())).host_key(HostKeyPolicy::KnownHosts(format!("{}/known_hosts", self.dir).into()));
        SshClient::connect(cfg).await.unwrap().into_session()
    }
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn scratch(name: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("kws-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

fn bytes(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(3);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

async fn wait_for(c: &mut Console, seen: &mut String, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !seen.contains(needle) {
        match rt_core::timeout(deadline.saturating_duration_since(Instant::now()), c.next()).await {
            Ok(Some(ConsoleEvent::Output(d))) => seen.push_str(&String::from_utf8_lossy(&d)),
            other => panic!("{other:?} while waiting for {needle:?}: {seen}"),
        }
    }
}

#[test]
fn consoles_sftp_and_commands_share_one_login() {
    let Some(l) = live() else { return };
    block(async move {
        let session = l.session().await;
        let mut first = session.console(PtyConfig::default()).await.unwrap();
        let mut second = session.console(PtyConfig { cols: 100, rows: 30, ..PtyConfig::default() }).await.unwrap();
        let mut sftp = session.sftp().await.unwrap();
        let name = scratch("shared");
        sftp.write(&name, b"written over sftp").await.unwrap();

        // Two terminals and SFTP open together, each its own.
        let (mut a, mut b) = (String::new(), String::new());
        first.write(format!("cat {name}; echo one-$((10+1))\n")).unwrap();
        second.write("stty size; echo two-$((20+2))\n").unwrap();
        wait_for(&mut second, &mut b, "two-22").await;
        wait_for(&mut first, &mut a, "one-11").await;
        assert!(a.contains("written over sftp"), "{a}");
        assert!(b.contains("30 100") && !b.contains("written over"), "{b}");

        // Commands alongside, and a big SFTP transfer while consoles stay live.
        assert_eq!(session.exec("echo alongside").await.unwrap().stdout_text(), "alongside\n");
        let big = bytes(4_000_000, 1);
        sftp.write(&name, &big).await.unwrap();
        first.write("echo still-$((3*3))\n").unwrap();
        wait_for(&mut first, &mut a, "still-9").await;
        assert!(sftp.read(&name).await.unwrap() == big, "4 MB over SFTP on the shared login");

        // Many commands at once, from separate tasks (OpenSSH allows MaxSessions = 10
        // channels per connection: two consoles + SFTP + six commands fit).
        let (tx, mut rx) = rt_core::mailbox::<(u32, String)>(64);
        for i in 0..6u32 {
            let (s, tx) = (session.clone(), tx.clone());
            rt_core::spawn(async move {
                let out = s.exec(&format!("sleep 0.$((RANDOM % 3)); echo task-{i}")).await.unwrap();
                tx.try_send((i, out.stdout_text())).unwrap();
            });
        }
        drop(tx);
        let mut done = Vec::new();
        while let Some((i, out)) = rx.recv().await {
            assert_eq!(out, format!("task-{i}\n"));
            done.push(i);
        }
        assert_eq!(done.len(), 6, "every concurrent command answered");

        sftp.remove_file(&name).await.unwrap();
        assert!(sftp.ssh().is_none(), "a shared SFTP session does not own the connection");
        // Closing one console leaves the other working.
        first.close().unwrap();
        second.write("echo after-close-$((4+4))\n").unwrap();
        wait_for(&mut second, &mut b, "after-close-8").await;

        session.disconnect();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.is_closed() && Instant::now() < deadline {
            rt_core::sleep(Duration::from_millis(20)).await;
        }
        assert!(session.is_closed());
        assert!(session.exec("echo x").await.is_err(), "nothing runs on a closed session");
        loop {
            match second.next().await {
                Some(ConsoleEvent::Closed(_)) | None => break,
                Some(_) => {}
            }
        }
    });
}

#[test]
fn a_local_forward_carries_an_ssh_connection() {
    let Some(l) = live() else { return };
    block(async move {
        let session = l.session().await;
        let fwd = session.forward_local("127.0.0.1:0".parse().unwrap(), "127.0.0.1", l.port).await.unwrap();
        let fp = session.host_key().fingerprint();
        // SSH inside SSH: a second login through the tunnel, data byte-exact.
        let mut inner = Vec::new();
        for _ in 0..2 {
            let cfg = SshConfig::new("127.0.0.1", &l.user).port(fwd.local_addr().port()).auth(Auth::Key(l.key())).host_key(HostKeyPolicy::Fingerprint(fp.clone()));
            inner.push(SshClient::connect(cfg).await.unwrap());
        }
        let data = bytes(2_000_000, 2);
        for c in &mut inner {
            assert!(c.exec_with_input("cat", &data).await.unwrap().stdout == data, "2 MB through the tunnel");
        }
        // A destination the server cannot reach: the connection is just closed.
        let dead = session.forward_local("127.0.0.1:0".parse().unwrap(), "127.0.0.1", 1).await.unwrap();
        let mut s = std::net::TcpStream::connect(dead.local_addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(s.read(&mut [0u8; 8]).unwrap_or(0), 0);
        fwd.stop();
        rt_core::sleep(Duration::from_millis(100)).await;
    });
}

#[test]
fn a_dynamic_forward_is_a_socks5_proxy() {
    let Some(l) = live() else { return };
    block(async move {
        let session = l.session().await;
        let fwd = session.forward_dynamic("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = fwd.local_addr();
        let port = l.port;
        // A SOCKS5 client in a thread (blocking I/O, as a browser would be).
        let probe = std::thread::spawn(move || {
            let socks = |host: &[u8], atyp: u8, p: u16| -> (u8, Vec<u8>) {
                let mut s = std::net::TcpStream::connect(addr).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                s.write_all(&[5, 1, 0]).unwrap();
                let mut r = [0u8; 2];
                s.read_exact(&mut r).unwrap();
                assert_eq!(r, [5, 0], "no authentication");
                let mut req = vec![5, 1, 0, atyp];
                if atyp == 3 {
                    req.push(host.len() as u8);
                }
                req.extend_from_slice(host);
                req.extend_from_slice(&p.to_be_bytes());
                s.write_all(&req).unwrap();
                let mut rep = [0u8; 10];
                s.read_exact(&mut rep).unwrap();
                let mut banner = vec![0u8; 8];
                if rep[1] == 0 {
                    s.read_exact(&mut banner).unwrap();
                }
                (rep[1], banner)
            };
            let by_name = socks(b"localhost", 3, port);
            let by_ip = socks(&[127, 0, 0, 1], 1, port);
            let refused = socks(&[127, 0, 0, 1], 1, 1);
            (by_name, by_ip, refused)
        });
        // Let the proxy's tasks run while the thread talks to it.
        let started = Instant::now();
        while !probe.is_finished() && started.elapsed() < Duration::from_secs(20) {
            rt_core::sleep(Duration::from_millis(10)).await;
        }
        let (by_name, by_ip, refused) = probe.join().unwrap();
        assert_eq!((by_name.0, by_name.1.as_slice()), (0, &b"SSH-2.0-"[..]), "a name, resolved on the server side");
        assert_eq!((by_ip.0, by_ip.1.as_slice()), (0, &b"SSH-2.0-"[..]));
        assert_ne!(refused.0, 0, "an unreachable destination is reported");
    });
}

#[test]
fn a_remote_forward_brings_server_connections_here() {
    let Some(l) = live() else { return };
    block(async move {
        // A service on this side: answers "pong:" + what it got, then closes.
        let svc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let svc_port = svc.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in svc.incoming().take(3) {
                let mut s = s.unwrap();
                let mut b = [0u8; 256];
                let n = s.read(&mut b).unwrap_or(0);
                let _ = s.write_all(&[b"pong:".as_slice(), &b[..n]].concat());
            }
        });
        let session = l.session().await;
        let fwd = session.forward_remote("127.0.0.1", 0, "127.0.0.1", svc_port).await.unwrap();
        assert!(fwd.port() > 0, "the server chose a port");
        for i in 0..3 {
            let out = session.exec(&format!("printf 'ping-{i}' | nc -w 5 127.0.0.1 {}", fwd.port())).await.unwrap();
            assert_eq!(out.stdout_text(), format!("pong:ping-{i}"), "{}", String::from_utf8_lossy(&out.stderr));
        }
        // A port the server cannot listen on (its own sshd has it) is refused with a reason.
        let e = session.forward_remote("127.0.0.1", l.port, "127.0.0.1", svc_port).await.unwrap_err();
        assert!(e.to_string().contains("refused"), "{e}");
    });
}

#[test]
fn sftp_as_a_remote_file_manager() {
    let Some(l) = live() else { return };
    block(async move {
        let session = l.session().await;
        let tmp = std::env::temp_dir().join(scratch("fm"));
        std::fs::create_dir_all(&tmp).unwrap();
        let big = bytes(20_000_000, 7);
        std::fs::write(tmp.join("big.bin"), &big).unwrap();

        // Two SFTP sessions on one login: a transfer in one task, browsing in another.
        let remote = scratch("fm-remote");
        let mut browse = session.sftp().await.unwrap();
        browse.create_dir_all(&format!("{remote}/sub/deeper")).await.unwrap();
        let (tx, mut rx) = rt_core::mailbox::<Vec<(u64, Option<u64>)>>(1);
        {
            let (s, local, target) = (session.clone(), tmp.join("big.bin"), format!("{remote}/big.bin"));
            rt_core::spawn(async move {
                let mut sftp = s.sftp().await.unwrap();
                let mut seen = Vec::new();
                let n = sftp.upload_from(&local, &target, |done, total| seen.push((done, total))).await.unwrap();
                assert_eq!(n, 20_000_000);
                tx.try_send(seen).unwrap();
            });
        }
        let mut listings = 0;
        let progress = loop {
            if let Ok(Some(p)) = rt_core::timeout(Duration::from_millis(5), rx.recv()).await {
                break p;
            }
            browse.read_dir(&remote).await.unwrap();
            listings += 1;
        };
        assert!(listings > 0, "browsing went on during the upload");
        assert!(progress.windows(2).all(|w| w[0].0 < w[1].0), "progress only goes up");
        assert_eq!(progress.last(), Some(&(20_000_000, Some(20_000_000))));

        // Streamed download back, then a tree both ways, a walk, a recursive delete.
        let mut steps = 0;
        let n = browse.download_to(&format!("{remote}/big.bin"), &tmp.join("back.bin"), |_, _| steps += 1).await.unwrap();
        assert_eq!(n, 20_000_000);
        assert!(steps >= 10, "reported as it went ({steps})");
        assert!(std::fs::read(tmp.join("back.bin")).unwrap() == big, "20 MB back byte-exact");

        let src = tmp.join("tree");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();
        std::fs::write(src.join("a/b/c.txt"), b"deep").unwrap();
        std::fs::write(src.join("tệp.txt"), "tiếng Việt").unwrap();
        assert_eq!(browse.upload_dir(&src, &format!("{remote}/tree")).await.unwrap(), 2);
        let walked: Vec<String> = browse.walk(&format!("{remote}/tree")).await.unwrap().into_iter().map(|(p, _)| p).collect();
        assert_eq!(walked, ["a", "a/b", "a/b/c.txt", "empty", "tệp.txt"]);
        assert_eq!(browse.download_dir(&format!("{remote}/tree"), &tmp.join("tree-back")).await.unwrap(), 2);
        assert_eq!(std::fs::read(tmp.join("tree-back/a/b/c.txt")).unwrap(), b"deep");
        assert!(tmp.join("tree-back/empty").is_dir());

        browse.symlink("tree", &format!("{remote}/link-to-tree")).await.unwrap();
        browse.remove_dir_all(&remote).await.unwrap();
        assert!(!browse.exists(&remote).await.unwrap(), "removed with everything in it");
        let _ = std::fs::remove_dir_all(&tmp);
    });
}
