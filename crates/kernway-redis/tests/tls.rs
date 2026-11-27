//! TLS (`rediss://`) against a real TLS-only Redis set up by
//! `tests/support/redis-tls-setup.sh` (`KERNWAY_REDIS_TLS_URL`,
//! `KERNWAY_REDIS_TLS_DIR` with ca.crt / client.crt / client.key). The server
//! certificate names DNS:localhost only.

use kernway_redis::{Connection, Options, RedisError, Value};
use std::time::Duration;

fn env() -> Option<(String, String)> {
    Some((std::env::var("KERNWAY_REDIS_TLS_URL").ok()?, std::env::var("KERNWAY_REDIS_TLS_DIR").ok()?))
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

async fn open(url: &str) -> Result<Connection, RedisError> {
    Connection::open(&Options::from_url(url)?).await
}

fn port(url: &str) -> &str {
    url.rsplit(':').next().unwrap()
}

#[test]
fn tls_certificate_checks() {
    let Some((url, dir)) = env() else { return eprintln!("skip: KERNWAY_REDIS_TLS_URL not set") };
    run(async move {
        let ca = format!("{dir}/ca.crt");
        let p = port(&url);
        // Full check, our CA, the certificate's name.
        let mut c = open(&format!("rediss://localhost:{p}?ssl_ca={ca}")).await.unwrap();
        assert_eq!(c.command(&[b"PING"]).await.unwrap(), Value::Simple("PONG".into()));
        let k = format!("kwtls:{}", std::process::id());
        let big = vec![7u8; 3 << 20];
        c.command(&[b"SET", k.as_bytes(), &big]).await.unwrap();
        assert_eq!(c.command(&[b"GET", k.as_bytes()]).await.unwrap().as_bulk().unwrap().len(), big.len(), "MBs through TLS");
        c.command(&[b"DEL", k.as_bytes()]).await.unwrap();

        // 127.0.0.1 is not in the certificate: Full refuses, CA-only accepts,
        // an explicit server name fixes it.
        let e = open(&format!("rediss://127.0.0.1:{p}?ssl_ca={ca}")).await.err().unwrap();
        assert!(matches!(&e, RedisError::Tls(m) if m.contains("not valid for name")), "{e}");
        open(&format!("rediss://127.0.0.1:{p}?ssl_ca={ca}&ssl_verify=ca")).await.unwrap();
        open(&format!("rediss://127.0.0.1:{p}?ssl_ca={ca}&ssl_server_name=localhost")).await.unwrap();

        // The public roots do not know our CA; `none` skips the check.
        let e = open(&format!("rediss://localhost:{p}")).await.err().unwrap();
        assert!(matches!(&e, RedisError::Tls(m) if m.to_lowercase().contains("unknown") && m.to_lowercase().contains("issuer")), "{e}");
        open(&format!("rediss://localhost:{p}?ssl_verify=none")).await.unwrap();

        // A CA file that is missing or not PEM is a configuration error.
        assert!(matches!(open(&format!("rediss://localhost:{p}?ssl_ca={dir}/missing.pem")).await, Err(RedisError::Config(_))));
        assert!(matches!(open(&format!("rediss://localhost:{p}?ssl_ca={dir}/server.ext")).await, Err(RedisError::Config(_))));
    });
}

#[test]
fn mutual_tls_and_plaintext_against_a_tls_port() {
    let Some((url, dir)) = env() else { return eprintln!("skip: KERNWAY_REDIS_TLS_URL not set") };
    run(async move {
        let p = port(&url);
        let base = format!("rediss://localhost:{p}?ssl_ca={dir}/ca.crt");
        let with_cert = format!("{base}&ssl_cert={dir}/client.crt&ssl_key={dir}/client.key");
        let mut admin = open(&with_cert).await.unwrap();
        admin.command(&[b"CONFIG", b"SET", b"tls-auth-clients", b"yes"]).await.unwrap();
        async {
            // Required: without a certificate the server ends the handshake.
            let without = async {
                let mut c = open(&base).await?;
                c.command(&[b"PING"]).await
            }
            .await;
            assert!(without.is_err(), "no client certificate: {without:?}");
            // With it, everything works.
            let mut c = open(&with_cert).await.unwrap();
            assert_eq!(c.command(&[b"PING"]).await.unwrap(), Value::Simple("PONG".into()));
        }
        .await;
        admin.command(&[b"CONFIG", b"SET", b"tls-auth-clients", b"optional"]).await.unwrap();

        // Plaintext to a TLS port: an error within the timeout, never a hang.
        let opts = Options::from_url(&format!("redis://localhost:{p}?timeout=2000&connect_timeout=2000")).unwrap();
        let r = rt_core::timeout(Duration::from_secs(5), Connection::open(&opts)).await.expect("no hang");
        assert!(r.is_err(), "plaintext against TLS");
    });
}
