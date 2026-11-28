//! The SSH and SFTP clients against a real OpenSSH server — set up by
//! `tests/support/ssh-setup.sh`, which prints `KERNWAY_SSH_*`. Without it each test
//! reports `skip:`, which the CI job and the acceptance run treat as a failure.
//! Password tests (`password_*`) need a server that can check passwords (the Docker
//! setup); a native, non-root sshd cannot, so those runs leave them out by name.

use kernway_ssh::*;
use std::time::Duration;

struct Live {
    host: String,
    port: u16,
    rekey_port: u16,
    user: String,
    password: String,
    dir: String,
}

fn live() -> Option<Live> {
    let Ok(host) = std::env::var("KERNWAY_SSH_HOST") else {
        eprintln!("skip: KERNWAY_SSH_HOST not set (tests/support/ssh-setup.sh)");
        return None;
    };
    let v = |k: &str| std::env::var(k).unwrap();
    Some(Live {
        host,
        port: v("KERNWAY_SSH_PORT").parse().unwrap(),
        rekey_port: v("KERNWAY_SSH_REKEY_PORT").parse().unwrap(),
        user: v("KERNWAY_SSH_USER"),
        password: v("KERNWAY_SSH_PASSWORD"),
        dir: v("KERNWAY_SSH_DIR"),
    })
}

impl Live {
    fn key(&self, name: &str) -> PrivateKey {
        PrivateKey::from_pem(&std::fs::read(format!("{}/keys/{name}", self.dir)).unwrap()).unwrap()
    }

    fn known_hosts(&self) -> HostKeyPolicy {
        HostKeyPolicy::KnownHosts(format!("{}/known_hosts", self.dir).into())
    }

    fn config(&self) -> SshConfig {
        SshConfig::new(&self.host, &self.user).port(self.port).auth(Auth::Key(self.key("id_ed25519"))).host_key(self.known_hosts())
    }

    async fn connect(&self) -> SshClient {
        SshClient::connect(self.config()).await.unwrap()
    }

    async fn sftp(&self) -> Sftp {
        self.connect().await.sftp().await.unwrap()
    }
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

/// A unique scratch name in the login directory.
fn scratch(name: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("kwt-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// Deterministic, not-compressible-looking bytes.
fn bytes(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
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
fn logs_in_with_every_key_type_and_format() {
    let Some(l) = live() else { return };
    block(async move {
        for name in ["id_ed25519", "id_ecdsa", "id_ecdsa384", "id_rsa", "id_rsa_pkcs8", "id_rsa_pem"] {
            let cfg = SshConfig::new(&l.host, &l.user).port(l.port).auth(Auth::Key(l.key(name))).host_key(l.known_hosts());
            let mut c = SshClient::connect(cfg).await.unwrap_or_else(|e| panic!("{name}: {e}"));
            let out = c.exec("id -un").await.unwrap();
            assert_eq!(out.stdout_text().trim(), l.user, "{name}");
            assert_eq!(out.exit_status, Some(0));
            assert!(c.server_version().starts_with("SSH-2.0-OpenSSH"), "{}", c.server_version());
        }
        assert!(std::fs::read_to_string(format!("{}/keys/id_rsa_pem", l.dir)).unwrap().contains("BEGIN RSA PRIVATE KEY"), "PKCS#1");
        assert!(std::fs::read_to_string(format!("{}/keys/id_rsa_pkcs8", l.dir)).unwrap().contains("BEGIN PRIVATE KEY"), "PKCS#8");
        // The public half matches what ssh-keygen wrote.
        let pubkey = std::fs::read_to_string(format!("{}/keys/id_ed25519.pub", l.dir)).unwrap();
        assert_eq!(PublicKey::from_openssh(&pubkey).unwrap(), *l.key("id_ed25519").public_key());
    });
}

#[test]
fn refused_logins_say_why() {
    let Some(l) = live() else { return };
    block(async move {
        let e = PrivateKey::from_pem(&std::fs::read(format!("{}/keys/id_ed25519_enc", l.dir)).unwrap()).unwrap_err();
        assert!(e.to_string().contains("passphrase"), "{e}");

        // A key the server does not know.
        let mut stranger = None;
        for _ in 0..1 {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", kernway_wire::base64::encode(pkcs8.as_ref()));
            stranger = Some(PrivateKey::from_pem(pem.as_bytes()).unwrap());
        }
        let cfg = SshConfig::new(&l.host, &l.user).port(l.port).auth(Auth::Key(stranger.unwrap())).host_key(l.known_hosts());
        let e = SshClient::connect(cfg).await.unwrap_err();
        assert!(matches!(&e, SshError::Auth { allowed } if allowed.iter().any(|m| m == "publickey")), "{e}");

        let cfg = SshConfig::new(&l.host, "no-such-user-kw").port(l.port).auth(Auth::Key(l.key("id_ed25519"))).host_key(l.known_hosts());
        assert!(matches!(SshClient::connect(cfg).await, Err(SshError::Auth { .. })));
        let cfg = SshConfig::new(&l.host, &l.user).port(l.port).host_key(l.known_hosts());
        assert!(matches!(SshClient::connect(cfg).await, Err(SshError::Auth { .. })), "no method given");
    });
}

#[test]
fn password_logins_with_password_and_keyboard_interactive() {
    let Some(l) = live() else { return };
    if l.password.is_empty() {
        eprintln!("skip: this server cannot check passwords (native sshd); run with SSH_DOCKER=1");
        return;
    }
    block(async move {
        let cfg = SshConfig::new(&l.host, &l.user).port(l.port).auth(Auth::Password(l.password.clone())).host_key(l.known_hosts());
        let mut c = SshClient::connect(cfg).await.unwrap();
        assert_eq!(c.exec("echo in").await.unwrap().stdout_text(), "in\n");
        let cfg = SshConfig::new(&l.host, &l.user).port(l.port).auth(Auth::Password("wrong".into())).host_key(l.known_hosts());
        assert!(matches!(SshClient::connect(cfg).await, Err(SshError::Auth { .. })));
        // A wrong password first, then the key: the second method still gets its turn.
        let cfg = SshConfig::new(&l.host, &l.user).port(l.port).auth(Auth::Password("wrong".into())).auth(Auth::Key(l.key("id_ecdsa"))).host_key(l.known_hosts());
        assert!(SshClient::connect(cfg).await.is_ok());
    });
}

#[test]
fn host_keys_are_checked_every_way() {
    let Some(l) = live() else { return };
    block(async move {
        // Each host key type the server has, each checked against known_hosts.
        for alg in ["ssh-ed25519", "ecdsa-sha2-nistp256", "rsa-sha2-512", "rsa-sha2-256"] {
            let c = SshClient::connect(l.config().host_key_algorithms(&[alg]).unwrap()).await.unwrap_or_else(|e| panic!("{alg}: {e}"));
            assert_eq!(c.negotiated().host_key, alg);
        }
        let c = l.connect().await;
        let fp = c.host_key().fingerprint();
        let real = c.host_key().clone();
        drop(c);

        // A pinned fingerprint: right, and wrong.
        assert!(SshClient::connect(l.config().host_key(HostKeyPolicy::Fingerprint(fp.clone()))).await.is_ok());
        let e = SshClient::connect(l.config().host_key(HostKeyPolicy::Fingerprint("SHA256:AAAA".into()))).await.unwrap_err();
        assert!(matches!(&e, SshError::HostKey(m) if m.contains(&fp)), "{e}");

        // Not listed: refused, naming the key so it can be checked.
        let e = SshClient::connect(l.config().host_key(HostKeyPolicy::KnownHostsText(String::new()))).await.unwrap_err();
        assert!(matches!(&e, SshError::HostKey(m) if m.contains("not in known_hosts") && m.contains(&fp)), "{e}");
        let e = SshClient::connect(l.config().host_key(HostKeyPolicy::KnownHosts(format!("{}/no-such-file", l.dir).into()))).await.unwrap_err();
        assert!(matches!(e, SshError::HostKey(_)), "{e}");

        // Listed with another key of the same type: an impostor, refused loudly.
        let impostor = PublicKey::Ed25519([7; 32]);
        let text = format!("[{}]:{} {}\n", l.host, l.port, impostor.to_openssh());
        let e = SshClient::connect(l.config().host_key(HostKeyPolicy::KnownHostsText(text)).host_key_algorithms(&["ssh-ed25519"]).unwrap()).await.unwrap_err();
        assert!(matches!(&e, SshError::HostKey(m) if m.contains("CHANGED")), "{e}");

        // Revoked.
        let all = std::fs::read_to_string(format!("{}/known_hosts", l.dir)).unwrap();
        let text = format!("@revoked * {}\n{all}", real.to_openssh());
        let e = SshClient::connect(l.config().host_key(HostKeyPolicy::KnownHostsText(text))).await.unwrap_err();
        assert!(matches!(&e, SshError::HostKey(m) if m.contains("revoked")), "{e}");

        // Hashed known_hosts (ssh-keygen -H), as many systems keep it.
        let hashed = format!("{}/known_hosts_hashed", l.dir);
        std::fs::copy(format!("{}/known_hosts", l.dir), &hashed).unwrap();
        let st = std::process::Command::new("ssh-keygen").args(["-q", "-H", "-f", &hashed]).status().unwrap();
        assert!(st.success());
        assert!(std::fs::read_to_string(&hashed).unwrap().contains("|1|"));
        assert!(SshClient::connect(l.config().host_key(HostKeyPolicy::KnownHosts(hashed.into()))).await.is_ok());

        // Known by ECDSA only: the client asks for ECDSA rather than failing on Ed25519.
        let ecdsa_only: String = all.lines().filter(|x| x.contains("ecdsa-sha2-nistp256")).map(|x| format!("{x}\n")).collect();
        let c = SshClient::connect(l.config().host_key(HostKeyPolicy::KnownHostsText(ecdsa_only))).await.unwrap();
        assert_eq!(c.negotiated().host_key, "ecdsa-sha2-nistp256");
    });
}

#[test]
fn every_key_exchange_and_cipher_works() {
    let Some(l) = live() else { return };
    block(async move {
        for kex in ["curve25519-sha256", "curve25519-sha256@libssh.org", "ecdh-sha2-nistp256", "ecdh-sha2-nistp384"] {
            for cipher in ["chacha20-poly1305@openssh.com", "aes256-gcm@openssh.com", "aes128-gcm@openssh.com"] {
                let cfg = l.config().kex_algorithms(&[kex]).unwrap().ciphers(&[cipher]).unwrap();
                let mut c = SshClient::connect(cfg).await.unwrap_or_else(|e| panic!("{kex} + {cipher}: {e}"));
                assert_eq!((c.negotiated().kex.as_str(), c.negotiated().cipher_out.as_str(), c.negotiated().cipher_in.as_str()), (kex, cipher, cipher));
                let data = bytes(200_000, 3);
                let out = c.exec_with_input("cat", &data).await.unwrap();
                assert!(out.stdout == data, "{kex} + {cipher}: 200 KB through cat, byte-exact");
            }
        }
        assert!(l.config().ciphers(&["aes128-cbc"]).is_err(), "an unsupported name is refused up front");
    });
}

#[test]
fn commands_return_output_status_and_signals() {
    let Some(l) = live() else { return };
    block(async move {
        let mut c = l.connect().await;
        let out = c.exec("printf 'xin chào\\n'; echo oops >&2; exit 7").await.unwrap();
        assert_eq!(out.stdout_text(), "xin chào\n");
        assert_eq!(out.stderr, b"oops\n");
        assert_eq!(out.exit_status, Some(7));
        let out = c.exec("kill -TERM $$").await.unwrap();
        assert_eq!((out.exit_status, out.exit_signal.as_deref()), (None, Some("TERM")));
        // Output far larger than the channel window: flow control in both directions.
        let out = c.exec("head -c 6000000 /dev/zero | tr '\\0' 'k'").await.unwrap();
        assert_eq!(out.stdout.len(), 6_000_000);
        assert!(out.stdout.iter().all(|&b| b == b'k'));
        let input = bytes(5_000_000, 9);
        let out = c.exec_with_input("cat", &input).await.unwrap();
        assert!(out.stdout == input, "5 MB stdin echoed byte-exact");
        let out = c.exec("sha256sum").await.unwrap();
        assert_eq!(out.exit_status, Some(0), "a command reading an empty stdin ends");
        // Many commands on one connection, one after the other.
        for i in 0..20 {
            assert_eq!(c.exec(&format!("echo {i}")).await.unwrap().stdout_text(), format!("{i}\n"));
        }
        c.disconnect().await.unwrap();
    });
}

#[test]
fn rekeying_started_by_either_side() {
    let Some(l) = live() else { return };
    block(async move {
        // Ours: after 64 KiB.
        let mut c = SshClient::connect(l.config().rekey_after_bytes(64 * 1024)).await.unwrap();
        let data = bytes(2_000_000, 5);
        assert!(c.exec_with_input("cat", &data).await.unwrap().stdout == data);
        assert!(c.rekeys() >= 10, "the client rekeyed {} times", c.rekeys());
        // The server's: RekeyLimit 64K on the second port.
        let cfg = SshConfig::new(&l.host, &l.user).port(l.rekey_port).auth(Auth::Key(l.key("id_ed25519"))).host_key(l.known_hosts());
        let mut c = SshClient::connect(cfg).await.unwrap();
        assert!(c.exec_with_input("cat", &data).await.unwrap().stdout == data);
        assert!(c.rekeys() >= 10, "the server rekeyed {} times", c.rekeys());
        let mut s = c.sftp().await.unwrap();
        let name = scratch("rekey");
        let big = bytes(4_000_000, 6);
        s.write(&name, &big).await.unwrap();
        assert!(s.read(&name).await.unwrap() == big, "4 MB over SFTP across rekeys");
        s.remove_file(&name).await.unwrap();
    });
}

#[test]
fn sftp_files_round_trip_byte_exact() {
    let Some(l) = live() else { return };
    block(async move {
        let mut s = l.sftp().await;
        assert_eq!(s.version(), 3);
        assert!(s.has_extension("posix-rename@openssh.com"));
        for (i, n) in [0usize, 1, 65_535, 65_536, 65_537, 1_000_000, 3_333_333].into_iter().enumerate() {
            let name = format!("{}-tệp-{n}.bin", scratch("rt"));
            let data = bytes(n, i as u32);
            s.write(&name, &data).await.unwrap();
            assert_eq!(s.metadata(&name).await.unwrap().size, Some(n as u64));
            assert!(s.read(&name).await.unwrap() == data, "{n} bytes byte-exact");
            s.remove_file(&name).await.unwrap();
        }
        // Overwriting truncates; appending appends.
        let name = scratch("over");
        s.write(&name, b"a long first version").await.unwrap();
        s.write(&name, b"short").await.unwrap();
        assert_eq!(s.read(&name).await.unwrap(), b"short");
        s.append(&name, b" + more").await.unwrap();
        assert_eq!(s.read(&name).await.unwrap(), b"short + more");
        s.remove_file(&name).await.unwrap();
        // Paths with spaces and UTF-8 are bytes on the wire, not mangled.
        let odd = "kwt dir ăâêôơư ✓";
        s.create_dir(odd).await.unwrap();
        s.write(&format!("{odd}/tên tệp.txt"), b"ok").await.unwrap();
        let names: Vec<String> = s.read_dir(odd).await.unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["tên tệp.txt"]);
        s.remove_file(&format!("{odd}/tên tệp.txt")).await.unwrap();
        s.remove_dir(odd).await.unwrap();
    });
}

#[test]
fn sftp_directories_links_and_attributes() {
    let Some(l) = live() else { return };
    block(async move {
        let mut s = l.sftp().await;
        let home = s.canonicalize(".").await.unwrap();
        assert!(home.starts_with('/'), "{home}");
        let root = scratch("tree");
        s.create_dir_all(&format!("{root}/a/b/c")).await.unwrap();
        s.create_dir_all(&format!("{root}/a/b")).await.unwrap();
        s.write(&format!("{root}/a/file.txt"), b"hello").await.unwrap();
        let mut list = s.read_dir(&format!("{root}/a")).await.unwrap();
        list.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(list.iter().map(|e| (e.name.as_str(), e.metadata.is_dir())).collect::<Vec<_>>(), [("b", true), ("file.txt", false)]);
        assert!(list[1].metadata.is_file() && list[1].metadata.size == Some(5));
        assert!(list[1].long_name.contains("file.txt"));

        // A directory that is not empty is not removed.
        let e = s.remove_dir(&format!("{root}/a")).await.unwrap_err();
        assert!(matches!(e, SshError::Sftp { status: SftpStatus::Failure, .. }), "{e}");

        // Rename over an existing file (POSIX semantics).
        s.write(&format!("{root}/x"), b"x").await.unwrap();
        s.write(&format!("{root}/y"), b"y").await.unwrap();
        s.rename(&format!("{root}/x"), &format!("{root}/y")).await.unwrap();
        assert_eq!(s.read(&format!("{root}/y")).await.unwrap(), b"x");
        assert!(!s.exists(&format!("{root}/x")).await.unwrap());

        // Symbolic and hard links.
        s.symlink("a/file.txt", &format!("{root}/link")).await.unwrap();
        assert_eq!(s.read_link(&format!("{root}/link")).await.unwrap(), "a/file.txt");
        assert!(s.symlink_metadata(&format!("{root}/link")).await.unwrap().is_symlink());
        assert!(s.metadata(&format!("{root}/link")).await.unwrap().is_file(), "stat follows the link");
        assert_eq!(s.read(&format!("{root}/link")).await.unwrap(), b"hello");
        s.hard_link(&format!("{root}/a/file.txt"), &format!("{root}/hard")).await.unwrap();
        assert_eq!(s.read(&format!("{root}/hard")).await.unwrap(), b"hello");

        // Permissions, times, size.
        let f = format!("{root}/a/file.txt");
        s.set_permissions(&f, 0o600).await.unwrap();
        assert_eq!(s.metadata(&f).await.unwrap().mode(), Some(0o600));
        s.set_times(&f, 1_000_000_000, 1_234_567_890).await.unwrap();
        let m = s.metadata(&f).await.unwrap();
        assert_eq!((m.atime, m.mtime), (Some(1_000_000_000), Some(1_234_567_890)));
        s.set_metadata(&f, &Metadata { size: Some(2), ..Metadata::default() }).await.unwrap();
        assert_eq!(s.read(&f).await.unwrap(), b"he");
        let stats = s.fs_stats(".").await.unwrap();
        assert!(stats.available_bytes() > 0 && stats.name_max >= 255, "{stats:?}");

        // Clean up through the same connection's shell.
        let out = s.ssh().unwrap().exec(&format!("rm -r '{root}' && echo gone")).await.unwrap();
        assert_eq!(out.stdout_text(), "gone\n");
        assert!(!s.exists(&root).await.unwrap());
    });
}

#[test]
fn sftp_handles_give_random_access() {
    let Some(l) = live() else { return };
    block(async move {
        let mut s = l.sftp().await;
        let name = scratch("rand");
        let f = s.open(&name, OpenOptions { read: true, write: true, create: true, truncate: true, ..OpenOptions::default() }).await.unwrap();
        s.write_at(&f, 0, b"0123456789").await.unwrap();
        s.write_at(&f, 3, b"abc").await.unwrap();
        s.write_at(&f, 20, b"end").await.unwrap();
        assert_eq!(s.read_at(&f, 0, 100).await.unwrap(), b"012abc6789\0\0\0\0\0\0\0\0\0\0end");
        assert_eq!(s.read_at(&f, 4, 3).await.unwrap(), b"bc6");
        assert!(s.read_at(&f, 1000, 10).await.unwrap().is_empty(), "past the end");
        assert_eq!(s.file_metadata(&f).await.unwrap().size, Some(23));
        s.sync(&f).await.unwrap();
        s.close(f).await.unwrap();
        let e = s.open(&name, OpenOptions { write: true, create_new: true, ..OpenOptions::default() }).await.unwrap_err();
        assert!(matches!(e, SshError::Sftp { .. }), "create_new on an existing file: {e}");
        s.remove_file(&name).await.unwrap();
    });
}

#[test]
fn sftp_errors_are_typed() {
    let Some(l) = live() else { return };
    block(async move {
        let mut s = l.sftp().await;
        let e = s.read("kwt-does-not-exist").await.unwrap_err();
        assert!(e.is_not_found(), "{e}");
        assert!(s.metadata("kwt-does-not-exist").await.unwrap_err().is_not_found());
        assert!(s.remove_file("kwt-does-not-exist").await.unwrap_err().is_not_found());
        assert!(!s.exists("kwt-does-not-exist").await.unwrap());
        // Permission denied: a file nobody may read, a directory nobody may write.
        let locked = scratch("locked");
        s.write(&locked, b"secret").await.unwrap();
        s.set_permissions(&locked, 0o000).await.unwrap();
        let e = s.read(&locked).await.unwrap_err();
        assert!(matches!(e, SshError::Sftp { status: SftpStatus::PermissionDenied, .. }), "{e}");
        let ro = scratch("ro");
        s.create_dir(&ro).await.unwrap();
        s.set_permissions(&ro, 0o500).await.unwrap();
        let e = s.write(&format!("{ro}/x"), b"x").await.unwrap_err();
        assert!(matches!(e, SshError::Sftp { status: SftpStatus::PermissionDenied, .. }), "{e}");
        s.set_permissions(&ro, 0o700).await.unwrap();
        s.remove_dir(&ro).await.unwrap();
        s.remove_file(&locked).await.unwrap();
        // The session is still fine after errors.
        assert!(s.canonicalize(".").await.is_ok());
        let ssh = s.into_ssh().await.unwrap();
        let mut ssh = ssh;
        assert_eq!(ssh.exec("echo after").await.unwrap().stdout_text(), "after\n");
    });
}

#[test]
fn connections_that_go_wrong_fail_cleanly() {
    let Some(l) = live() else { return };
    block(async move {
        // Nothing listening.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let e = SshClient::connect(l.config().port(free)).await.unwrap_err();
        assert!(matches!(e, SshError::Io(_)), "{e}");
        // Something that never speaks SSH: the connect timeout ends it.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let started = std::time::Instant::now();
        let e = SshClient::connect(l.config().port(port).connect_timeout(Duration::from_millis(700))).await.unwrap_err();
        assert!(matches!(e, SshError::Timeout), "{e}");
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(silent);
    });
}
