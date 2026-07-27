//! Echo server on the Kernway runtime — the v0.2 benchmark target.
//!
//! One shard per core, each with its own `SO_REUSEPORT` listener, executor and
//! reactor. Connections never move between shards.
//!
//! ```text
//! cargo run --release -p echo-server            # 0.0.0.0:9000, one shard per CPU
//! cargo run --release -p echo-server 9001 2     # port 9001, two shards
//!
//! nc 127.0.0.1 9000                             # type a line, get it back
//! ```

use std::io;

use rt_net::{run_shards, AsyncTcpStream, ShardConfig};

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args
        .next()
        .map(|p| p.parse().expect("port must be a number"))
        .unwrap_or(9000);
    let shards = args
        .next()
        .map(|s| s.parse().expect("shard count must be a number"));

    let mut config = ShardConfig::new(format!("0.0.0.0:{port}").parse().unwrap());
    if let Some(shards) = shards {
        config = config.shards(shards);
    }

    println!("kernway echo — {} shard(s) on port {port}", config.shards);
    if !rt_net::balances_reuseport() {
        println!(
            "note: this platform does not load-balance SO_REUSEPORT, so one shard \
             will take most connections (Linux does balance)"
        );
    }
    println!("try:  nc 127.0.0.1 {port}");

    run_shards(config, echo)
}

/// Echo until the peer hangs up. Returns on EOF or any I/O error — a dead
/// connection is normal traffic, not something to report.
async fn echo(mut stream: AsyncTcpStream) {
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if stream.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}
