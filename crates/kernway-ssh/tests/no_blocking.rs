//! The SSH and SFTP clients are fully async on kernway's reactor: no blocking system call
//! may run on a shard. This scans the driver's source for the blocking APIs —
//! DNS lookups, file reads, `std` sockets, sleeps — and fails on any use that
//! is not inside a `spawn_blocking` closure (the blocking pool).

use std::path::Path;

/// Blocking APIs that must never run on the shard.
const BLOCKING: &[&str] = &[
    "to_socket_addrs",
    "std::fs::",
    "std::net::TcpStream",
    "std::os::unix::net",
    "std::thread::sleep",
    "File::open",
    ".read_to_string(",
    "std::io::stdin",
];

/// Lines above a match that may hold the `spawn_blocking(` it runs in.
const WINDOW: usize = 4;

#[test]
fn no_blocking_call_runs_on_the_reactor() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        // A Windows checkout has CRLF line ends.
        let text = std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
        // Unit tests may block: they are not driver code.
        let code = text.split("#[cfg(test)]\nmod tests").next().unwrap_or(&text);
        let lines: Vec<&str> = code.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for api in BLOCKING {
                if line.contains(api) {
                    let from = i.saturating_sub(WINDOW);
                    if !lines[from..=i].iter().any(|l| l.contains("spawn_blocking(")) {
                        offenders.push(format!("{}:{}: {}", path.file_name().unwrap().to_string_lossy(), i + 1, trimmed));
                    }
                }
            }
        }
    }
    assert!(offenders.is_empty(), "blocking calls on the reactor:\n{}", offenders.join("\n"));
}
