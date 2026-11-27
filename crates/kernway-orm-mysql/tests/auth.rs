//! Authentication edge cases against a real MySQL 8 server:
//! `caching_sha2_password` full authentication over plaintext (RSA-encrypted
//! password) and mutual TLS (`REQUIRE X509`).
//!
//! Needs `KERNWAY_MYSQL_ROOT_URL` (to create the test accounts and clear the
//! auth cache) and `KERNWAY_MYSQL_CERTS`: a directory with the server's
//! `ca.pem`, `client-cert.pem`, `client-key.pem` and `public_key.pem` (MySQL
//! generates them in its data directory).
#![cfg(feature = "mysql")]

use kernway_orm_mysql::{params, MySqlDriver, Value};

struct Env {
    root: String,
    host: String,
    certs: std::path::PathBuf,
}

fn env() -> Option<Env> {
    let root = std::env::var("KERNWAY_MYSQL_ROOT_URL").ok()?;
    let certs = std::env::var("KERNWAY_MYSQL_CERTS").ok()?.into();
    let host = root.split('@').nth(1)?.split('/').next()?.to_string();
    Some(Env { root, host, certs })
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

const RSA_PW: &str = "rsa-pw-with-a-long-tail-0123456789-abcdefghijklmnopqrstuvwxyz-ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Create a test account once per run (tests run in parallel). Each test
/// creates only its own: MariaDB has no `caching_sha2_password`, so the RSA
/// account cannot exist there, while the X509 one can.
/// (Each test runs on its own thread with its own executor, so holding the
/// lock across the await only serializes the setups.)
#[allow(clippy::await_holding_lock)]
async fn setup(admin: &MySqlDriver, script: &str) {
    static DONE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut done = DONE.lock().unwrap();
    if done.iter().any(|s| s == script) {
        return;
    }
    done.push(script.to_string());
    admin.execute_script(script).await.unwrap();
}

fn rsa_account() -> String {
    format!(
        "DROP USER IF EXISTS 'kw_rsa'@'%'; CREATE USER 'kw_rsa'@'%' IDENTIFIED WITH caching_sha2_password BY '{RSA_PW}';
         GRANT SELECT ON kernway_test.* TO 'kw_rsa'@'%';"
    )
}

const X509_ACCOUNT: &str = "DROP USER IF EXISTS 'kw_x509'@'%'; CREATE USER 'kw_x509'@'%' IDENTIFIED BY 'x509_pw' REQUIRE X509;
     GRANT SELECT ON kernway_test.* TO 'kw_x509'@'%';";

/// Empty the server's `caching_sha2_password` cache so the next login must
/// do a full authentication.
async fn clear_auth_cache(admin: &MySqlDriver) {
    admin.execute("FLUSH PRIVILEGES").await.unwrap();
}

async fn cipher(db: &MySqlDriver) -> String {
    let r = db.query("SHOW SESSION STATUS LIKE 'Ssl_cipher'", &[]).await.unwrap();
    r.rows[0][1].as_str().unwrap_or_default().to_string()
}

#[test]
fn caching_sha2_full_auth_without_tls_uses_the_servers_rsa_key() {
    let Some(e) = env() else { return eprintln!("skip: KERNWAY_MYSQL_ROOT_URL / KERNWAY_MYSQL_CERTS not set") };
    run(async move {
        let admin = MySqlDriver::connect(&e.root).await.unwrap();
        setup(&admin, &rsa_account()).await;
        let base = format!("mysql://kw_rsa:{RSA_PW}@{}/kernway_test?ssl-mode=disabled", e.host);

        // Without a key and without permission to fetch one: a clear error.
        clear_auth_cache(&admin).await;
        let err = MySqlDriver::connect(&base).await.err().expect("must refuse").to_string();
        assert!(err.contains("server-public-key"), "{err}");

        // Fetching the key from the server (explicitly allowed).
        clear_auth_cache(&admin).await;
        let db = MySqlDriver::connect(&format!("{base}&get-server-public-key=true")).await.unwrap();
        assert_eq!(cipher(&db).await, "", "really plaintext");
        assert_eq!(db.query_scalar("SELECT CURRENT_USER()", &[]).await.unwrap(), Value::Text("kw_rsa@%".into()));

        // The key from a file.
        clear_auth_cache(&admin).await;
        let key = e.certs.join("public_key.pem");
        let db = MySqlDriver::connect(&format!("{base}&server-public-key={}", key.display())).await.unwrap();
        assert_eq!(db.query_scalar("SELECT ?", params![7]).await.unwrap(), Value::Int(7));

        // A wrong password is the server's "access denied", not a protocol error.
        clear_auth_cache(&admin).await;
        let bad = format!("mysql://kw_rsa:wrong@{}/kernway_test?ssl-mode=disabled&get-server-public-key=true", e.host);
        let err = MySqlDriver::connect(&bad).await.err().expect("must fail").to_string();
        assert!(err.contains("1045"), "{err}");
    });
}

#[test]
fn mutual_tls_presents_the_client_certificate() {
    let Some(e) = env() else { return eprintln!("skip: KERNWAY_MYSQL_ROOT_URL / KERNWAY_MYSQL_CERTS not set") };
    run(async move {
        let admin = MySqlDriver::connect(&e.root).await.unwrap();
        setup(&admin, X509_ACCOUNT).await;
        let c = |f: &str| e.certs.join(f).display().to_string();
        let base = format!("mysql://kw_x509:x509_pw@{}/kernway_test?ssl-mode=verify_ca&ssl-ca={}", e.host, c("ca.pem"));
        // REQUIRE X509 without a certificate: refused.
        let err = MySqlDriver::connect(&base).await.err().expect("must refuse").to_string();
        assert!(err.contains("1045"), "{err}");
        // With the client certificate.
        let db = MySqlDriver::connect(&format!("{base}&ssl-cert={}&ssl-key={}", c("client-cert.pem"), c("client-key.pem")))
            .await
            .unwrap();
        assert!(!cipher(&db).await.is_empty(), "TLS in use");
        assert_eq!(db.query_scalar("SELECT CURRENT_USER()", &[]).await.unwrap(), Value::Text("kw_x509@%".into()));
        // A certificate without its key is a configuration error.
        assert!(MySqlDriver::connect(&format!("{base}&ssl-cert={}", c("client-cert.pem"))).await.is_err());
    });
}

/// MariaDB only (`KERNWAY_MARIADB_VERSION`, from `mariadb-setup.sh`, which
/// installs the `auth_ed25519` and `auth_parsec` plugins).
fn mariadb() -> Option<Env> {
    std::env::var("KERNWAY_MARIADB_VERSION").ok()?;
    env()
}

const ED25519_ACCOUNT: &str = "DROP USER IF EXISTS 'kw_ed'@'%'; CREATE USER 'kw_ed'@'%' IDENTIFIED VIA ed25519 USING PASSWORD('ed-pw-0123456789-a-longer-password');
     GRANT SELECT ON kernway_test.* TO 'kw_ed'@'%';";
const PARSEC_ACCOUNT: &str = "DROP USER IF EXISTS 'kw_parsec'@'%'; CREATE USER 'kw_parsec'@'%' IDENTIFIED VIA parsec USING PASSWORD('parsec-pw');
     GRANT SELECT ON kernway_test.* TO 'kw_parsec'@'%';";

/// Log in as `user` with plaintext and with TLS, run a query on each, and
/// check that a wrong password is the server's "access denied".
async fn logs_in(e: &Env, user: &str, pw: &str) {
    let ca = e.certs.join("ca.pem").display().to_string();
    for mode in ["ssl-mode=disabled".to_string(), format!("ssl-mode=verify_ca&ssl-ca={ca}")] {
        for _ in 0..3 {
            // Fresh scrambles each time: the signature changes, the login holds.
            let db = MySqlDriver::connect(&format!("mysql://{user}:{pw}@{}/kernway_test?{mode}", e.host)).await.unwrap_or_else(|err| panic!("{user} ({mode}): {err}"));
            assert_eq!(db.query_scalar("SELECT CURRENT_USER()", &[]).await.unwrap(), Value::Text(format!("{user}@%")));
            assert_eq!(db.query_scalar("SELECT ?", params![42]).await.unwrap(), Value::Int(42));
        }
        let bad = format!("mysql://{user}:not-{pw}@{}/kernway_test?{mode}", e.host);
        let err = MySqlDriver::connect(&bad).await.err().expect("a wrong password must fail").to_string();
        assert!(err.contains("1045"), "{user} ({mode}): {err}");
    }
}

#[test]
fn mariadb_ed25519_logs_in_and_refuses_a_wrong_password() {
    let Some(e) = mariadb() else { return eprintln!("skip: KERNWAY_MARIADB_VERSION not set (mariadb-setup.sh)") };
    run(async move {
        let admin = MySqlDriver::connect(&e.root).await.unwrap();
        setup(&admin, ED25519_ACCOUNT).await;
        let plugin = admin.query_scalar("SELECT plugin FROM mysql.user WHERE user = 'kw_ed'", &[]).await.unwrap();
        assert_eq!(plugin, Value::Text("ed25519".into()), "the account really uses ed25519");
        logs_in(&e, "kw_ed", "ed-pw-0123456789-a-longer-password").await;
    });
}

#[test]
fn mariadb_parsec_logs_in_and_refuses_a_wrong_password() {
    let Some(e) = mariadb() else { return eprintln!("skip: KERNWAY_MARIADB_VERSION not set (mariadb-setup.sh)") };
    run(async move {
        let admin = MySqlDriver::connect(&e.root).await.unwrap();
        setup(&admin, PARSEC_ACCOUNT).await;
        let plugin = admin.query_scalar("SELECT plugin FROM mysql.user WHERE user = 'kw_parsec'", &[]).await.unwrap();
        assert_eq!(plugin, Value::Text("parsec".into()), "the account really uses parsec");
        logs_in(&e, "kw_parsec", "parsec-pw").await;
    });
}
