//! Port forwarding over a shared session (RFC 4254 §7): local (`ssh -L`), dynamic
//! (`ssh -D`, a SOCKS5 proxy) and remote (`ssh -R`). Each forwarded connection is a
//! channel; two tasks copy bytes between it and the TCP socket, each direction
//! closing on its own end (half-close travels as EOF).

use crate::session::{Channel, ChannelEvent, SshSession};
use crate::SshError;
use rt_core::{MailboxReceiver, Shutdown};
use rt_net::{AsyncTcpListener, AsyncTcpStream};
use std::net::SocketAddr;

/// A local or dynamic forward that is listening. Dropping it (or [`stop`](Self::stop))
/// stops accepting; connections already carried finish on their own.
#[derive(Debug)]
pub struct Forward {
    local_addr: SocketAddr,
    stop: Shutdown,
}

impl Forward {
    /// Where it listens (the real port when asked for port 0).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting connections.
    pub fn stop(&self) {
        self.stop.trigger();
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.stop.trigger();
    }
}

/// A remote forward the server is listening for.
#[derive(Debug)]
pub struct RemoteForward {
    port: u16,
    stop: Shutdown,
}

impl RemoteForward {
    /// The port the server listens on (the one it chose when asked for 0).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop carrying new connections (the server keeps the port until the session ends).
    pub fn stop(&self) {
        self.stop.trigger();
    }
}

impl Drop for RemoteForward {
    fn drop(&mut self) {
        self.stop.trigger();
    }
}

#[derive(Clone)]
pub(crate) enum Target {
    Fixed(String, u16),
    Socks,
}

pub(crate) async fn local(session: SshSession, listen: SocketAddr, target: Target) -> Result<Forward, SshError> {
    let mut listener = AsyncTcpListener::bind(listen)?;
    let local_addr = listener.local_addr()?;
    let stop = Shutdown::new();
    let s2 = stop.clone();
    rt_core::spawn(async move {
        while let Some(accepted) = rt_core::until_shutdown(&s2, listener.accept()).await {
            match accepted {
                Ok((sock, peer)) => {
                    rt_core::spawn(serve(session.clone(), sock, peer, target.clone()));
                }
                Err(_) => rt_core::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    });
    Ok(Forward { local_addr, stop })
}

pub(crate) fn remote(port: u16, mut accept: MailboxReceiver<Channel>, host: String, to_port: u16) -> RemoteForward {
    let stop = Shutdown::new();
    let s2 = stop.clone();
    rt_core::spawn(async move {
        while let Some(Some(ch)) = rt_core::until_shutdown(&s2, accept.recv()).await {
            let host = host.clone();
            rt_core::spawn(async move {
                match kernway_wire::offload::connect_tcp(&host, to_port).await {
                    Ok(sock) => pump(ch, sock).await,
                    Err(_) => {
                        let _ = ch.writer().close();
                    }
                }
            });
        }
    });
    RemoteForward { port, stop }
}

async fn serve(session: SshSession, mut sock: AsyncTcpStream, peer: SocketAddr, target: Target) {
    sock.set_nodelay(true).ok();
    let socks = matches!(target, Target::Socks);
    let (host, port) = match target {
        Target::Fixed(h, p) => (h, p),
        Target::Socks => match socks_request(&mut sock).await {
            Ok(dest) => dest,
            Err(_) => return,
        },
    };
    match session.open_direct(&host, port, &peer.ip().to_string(), peer.port()).await {
        Ok(ch) => {
            // SOCKS5: "succeeded" (the bound address is not meaningful through SSH).
            if socks && sock.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.is_err() {
                return;
            }
            pump(ch, sock).await;
        }
        Err(_) => {
            if socks {
                // "Connection refused" — the server could not reach the destination.
                let _ = sock.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            }
        }
    }
}

async fn read_exact(sock: &mut AsyncTcpStream, n: usize) -> Result<Vec<u8>, SshError> {
    let mut out = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let k = sock.read(&mut out[got..]).await?;
        if k == 0 {
            return Err(SshError::Channel("the SOCKS client went away".into()));
        }
        got += k;
    }
    Ok(out)
}

/// The SOCKS5 handshake (RFC 1928), no authentication, CONNECT only: the destination.
async fn socks_request(sock: &mut AsyncTcpStream) -> Result<(String, u16), SshError> {
    let bad = |m: &str| SshError::Channel(format!("SOCKS: {m}"));
    let head = read_exact(sock, 2).await?;
    if head[0] != 5 {
        return Err(bad("not SOCKS version 5"));
    }
    let methods = read_exact(sock, head[1] as usize).await?;
    if !methods.contains(&0) {
        sock.write_all(&[5, 0xff]).await?;
        return Err(bad("the client offers no method without authentication"));
    }
    sock.write_all(&[5, 0]).await?;
    let req = read_exact(sock, 4).await?;
    if req[0] != 5 || req[1] != 1 {
        sock.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(bad("only CONNECT is supported"));
    }
    let host = match req[3] {
        1 => {
            let a = read_exact(sock, 4).await?;
            std::net::Ipv4Addr::new(a[0], a[1], a[2], a[3]).to_string()
        }
        3 => {
            let len = read_exact(sock, 1).await?[0] as usize;
            String::from_utf8_lossy(&read_exact(sock, len).await?).into_owned()
        }
        4 => {
            let a: [u8; 16] = read_exact(sock, 16).await?.try_into().expect("16 bytes");
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => {
            sock.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(bad("an unknown address type"));
        }
    };
    let p = read_exact(sock, 2).await?;
    Ok((host, u16::from_be_bytes([p[0], p[1]])))
}

/// Copy between a channel and a socket. Each direction ends on its own (an EOF
/// from either side is passed on as a half-close); the channel is closed once both
/// have ended — or at once when the other end closes it.
pub(crate) async fn pump(ch: Channel, sock: AsyncTcpStream) {
    use std::cell::Cell;
    use std::rc::Rc;
    let (writer, mut events) = ch.split();
    let (mut rd, mut wr) = sock.into_split();
    // Two directions; whichever finishes second closes the channel.
    let open_directions = Rc::new(Cell::new(2u8));
    let finish = |left: &Rc<Cell<u8>>, w: &crate::session::ChannelWriter| {
        left.set(left.get() - 1);
        if left.get() == 0 {
            let _ = w.close();
        }
    };
    {
        let (w2, left) = (writer.clone(), open_directions.clone());
        rt_core::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => {
                        let _ = w2.send_eof();
                        break;
                    }
                    Ok(n) => {
                        if w2.write_waiting(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            finish(&left, &w2);
        });
    }
    let mut closed = false;
    while let Some(e) = events.recv().await {
        match e {
            ChannelEvent::Data(d) => {
                if wr.write_all(&d).await.is_err() {
                    break;
                }
            }
            ChannelEvent::Eof => break,
            ChannelEvent::Closed(_) => {
                closed = true;
                break;
            }
            _ => {}
        }
    }
    let _ = wr.shutdown(std::net::Shutdown::Write);
    if closed {
        let _ = writer.close();
    } else {
        finish(&open_directions, &writer);
        // Keep taking events (the final Closed) so the session can forget the channel.
        rt_core::spawn(async move { while events.recv().await.is_some() {} });
    }
}
