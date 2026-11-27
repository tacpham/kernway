//! Redis Sentinel: ask the sentinels where a group's master (or a replica)
//! is, check the node really has that role, and connect to it.

use crate::conn::Connection;
use crate::error::RedisError;
use crate::options::{Endpoint, Options, Protocol, SentinelOptions};
use crate::resp::Value;

/// Open a data connection through Sentinel. Each sentinel is asked in turn;
/// one that is down, does not know the group, or names a node that turns out
/// not to have the expected role is skipped.
pub(crate) async fn open(opts: &Options, sen: &SentinelOptions) -> Result<Connection, RedisError> {
    let mut errors = Vec::new();
    for (host, port) in &sen.sentinels {
        match ask(opts, sen, host, *port).await {
            Ok(candidates) => {
                for (h, p) in candidates {
                    match connect_node(opts, sen, &h, p).await {
                        Ok(c) => return Ok(c),
                        Err(e) => errors.push(format!("{h}:{p} (from sentinel {host}:{port}): {e}")),
                    }
                }
                if !sen.replica {
                    continue;
                }
                errors.push(format!("sentinel {host}:{port}: no usable replica of {}", sen.master));
            }
            Err(e) => errors.push(format!("sentinel {host}:{port}: {e}")),
        }
    }
    Err(RedisError::Config(format!("no {} of {} reachable through Sentinel: {}", role(sen), sen.master, errors.join("; "))))
}

fn role(sen: &SentinelOptions) -> &'static str {
    if sen.replica { "replica" } else { "master" }
}

/// The addresses one sentinel gives for the group.
async fn ask(opts: &Options, sen: &SentinelOptions, host: &str, port: u16) -> Result<Vec<(String, u16)>, RedisError> {
    let mut so = Options::tcp(host, port).protocol(Protocol::Resp2).connect_timeout(opts.connect_timeout);
    so.tls = opts.tls.clone();
    so.username = sen.username.clone();
    so.password = sen.password.clone();
    so.command_timeout = Some(opts.connect_timeout);
    let mut s = Connection::open(&so).await?;
    if !sen.replica {
        return match s.command(&[b"SENTINEL", b"GET-MASTER-ADDR-BY-NAME", sen.master.as_bytes()]).await? {
            Value::Array(a) if a.len() == 2 => Ok(vec![(text(&a[0])?, text(&a[1])?.parse().map_err(|_| bad("port"))?)]),
            Value::Nil => Err(RedisError::Config(format!("sentinel does not monitor {:?}", sen.master))),
            other => Err(RedisError::unexpected("SENTINEL GET-MASTER-ADDR-BY-NAME", &other)),
        };
    }
    let reply = s.command(&[b"SENTINEL", b"REPLICAS", sen.master.as_bytes()]).await?;
    let Value::Array(replicas) = reply else {
        return Err(RedisError::unexpected("SENTINEL REPLICAS", &reply));
    };
    let mut out = Vec::new();
    for r in replicas {
        let fields: Vec<(String, String)> = r
            .into_pairs()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| Some((k.as_text()?, v.as_text()?)))
            .collect();
        let get = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.as_str());
        let healthy = get("flags").is_some_and(|f| !f.contains("s_down") && !f.contains("o_down") && !f.contains("disconnected"));
        if let (true, Some(ip), Some(port)) = (healthy, get("ip"), get("port").and_then(|p| p.parse().ok())) {
            out.push((ip.to_string(), port));
        }
    }
    Ok(out)
}

/// Connect to a node and check its role: a sentinel can be briefly behind a
/// failover and name a node that is no longer the master.
async fn connect_node(opts: &Options, sen: &SentinelOptions, host: &str, port: u16) -> Result<Connection, RedisError> {
    let mut o = opts.clone();
    o.endpoint = Endpoint::Tcp { host: host.to_string(), port };
    let mut c = Connection::open(&o).await?;
    let role_reply = c.command(&[b"ROLE"]).await?;
    let actual = role_reply.as_items().and_then(|i| i.first()).and_then(Value::as_text).unwrap_or_default();
    let want = if sen.replica { "slave" } else { "master" };
    if actual != want {
        return Err(RedisError::Config(format!("{host}:{port} is a {actual}, not a {want}")));
    }
    // Only a master connection is re-resolved on READONLY: a replica
    // connection refuses writes by design.
    c.sentinel_managed = !sen.replica;
    Ok(c)
}

fn text(v: &Value) -> Result<String, RedisError> {
    v.as_text().ok_or_else(|| RedisError::unexpected("SENTINEL", v))
}

fn bad(what: &str) -> RedisError {
    RedisError::Protocol(format!("sentinel sent a bad {what}"))
}
