//! One connection to one broker: TLS, the ApiVersions check, SASL, then an
//! actor that pipelines requests and matches responses by correlation id.

use crate::options::{Options, Sasl};
use crate::protocol::{request, response_body, Api, KafkaCode, R, W};
use kernway_messaging::MsgError;
use kernway_wire::BufStream;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};
use std::time::Duration;

/// Largest response frame accepted (fetches are capped well below).
const MAX_FRAME: usize = 256 << 20;

fn net(e: impl std::fmt::Display) -> MsgError {
    MsgError::Connection(e.to_string())
}

#[derive(Default)]
struct Slot {
    reply: Option<Result<Vec<u8>, MsgError>>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct State {
    out: Vec<u8>,
    next_corr: i32,
    pending: HashMap<i32, Slot>,
    closed: Option<MsgError>,
    writer: Option<Waker>,
}

struct Inner {
    client_id: String,
    request_timeout: Duration,
    st: RefCell<State>,
}

/// A connection to one broker. Cheap to clone; `!Send`.
#[derive(Clone)]
pub struct BrokerConn {
    inner: Rc<Inner>,
    /// `host:port`, for messages.
    pub addr: String,
}

async fn read_frame(io: &mut BufStream) -> Result<Vec<u8>, MsgError> {
    io.fill(4).await.map_err(net)?;
    let n = i32::from_be_bytes(io.peek(4).try_into().unwrap_or_default());
    if n < 4 || n as usize > MAX_FRAME {
        return Err(MsgError::Protocol(format!("Kafka: response frame of {n} bytes")));
    }
    let total = 4 + n as usize;
    io.fill(total).await.map_err(net)?;
    Ok(io.take(total)[4..].to_vec())
}

/// A request on the bare stream (the handshake, before the actor runs).
async fn direct(io: &mut BufStream, api: Api, corr: i32, client_id: &str, body: &[u8]) -> Result<Vec<u8>, MsgError> {
    io.write_buf().extend_from_slice(&request(api, corr, client_id, body));
    io.flush().await.map_err(net)?;
    let frame = read_frame(io).await?;
    let (c, b) = response_body(api, &frame)?;
    if c != corr {
        return Err(MsgError::Protocol(format!("Kafka: {} answered with correlation {c}, not {corr}", api.name())));
    }
    Ok(b.to_vec())
}

async fn tls_config(opts: &Options) -> Result<Option<std::sync::Arc<kernway_tls::rustls::ClientConfig>>, MsgError> {
    let Some(t) = &opts.tls else { return Ok(None) };
    let read = |p: &std::path::Path| {
        let p = p.to_path_buf();
        async move { kernway_wire::offload::read_file(&p).await.map_err(|e| MsgError::Config(format!("read {}: {e}", p.display()))) }
    };
    let ca = match &t.ca_file {
        Some(p) => Some(read(p).await?),
        None => None,
    };
    let id = match (&t.cert_file, &t.key_file) {
        (Some(c), Some(k)) => Some((read(c).await?, read(k).await?)),
        _ => None,
    };
    let check = if t.insecure { kernway_tls::CertCheck::None } else { kernway_tls::CertCheck::Full };
    let ident = id.as_ref().map(|(c, k)| kernway_tls::ClientIdentity { cert_pem: c, key_pem: k });
    kernway_tls::client_config_with_identity(ca.as_deref(), check, ident).map(Some).map_err(|e| MsgError::Config(format!("TLS: {e}")))
}

/// Check the broker speaks every API version the client uses.
fn check_versions(body: &[u8]) -> Result<(), MsgError> {
    let mut r = R::new(body, true);
    let code = r.i16()?;
    if code != 0 {
        return Err(MsgError::Broker(format!(
            "the broker refused ApiVersions v3 ({}): Kafka 2.4 or newer is needed",
            KafkaCode(code).name()
        )));
    }
    let n = r.array_len()?;
    let mut have = HashMap::new();
    for _ in 0..n {
        let (key, min, max) = (r.i16()?, r.i16()?, r.i16()?);
        r.tags()?;
        have.insert(key, (min, max));
    }
    let mut missing = Vec::new();
    for api in Api::ALL {
        let (key, v, _) = api.spec();
        match have.get(&key) {
            Some(&(min, max)) if min <= v && v <= max => {}
            Some(&(min, max)) => missing.push(format!("{} v{v} (broker: v{min}–v{max})", api.name())),
            None => missing.push(format!("{} (not offered)", api.name())),
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(MsgError::Broker(format!("the broker lacks API versions this client needs (Kafka 3.0+): {}", missing.join(", "))))
    }
}

async fn authenticate(io: &mut BufStream, opts: &Options, (mech, user, pass): &(Sasl, String, String), corr: &mut i32) -> Result<(), MsgError> {
    let mut w = W::new(false);
    w.string(mech.name());
    *corr += 1;
    let b = direct(io, Api::SaslHandshake, *corr, &opts.client_id, &w.b).await?;
    let mut r = R::new(&b, false);
    let code = r.i16()?;
    if code != 0 {
        let n = r.array_len()?;
        let offered: Vec<String> = (0..n).map(|_| r.string()).collect::<Result<_, _>>()?;
        return Err(MsgError::Auth(format!("the broker does not offer SASL {} (it offers: {})", mech.name(), offered.join(", "))));
    }
    let round = |bytes: Vec<u8>, corr: i32| {
        let mut w = W::new(true);
        w.bytes(&bytes).tags();
        (w.b, corr)
    };
    let auth = |b: &[u8]| -> Result<Vec<u8>, MsgError> {
        let mut r = R::new(b, true);
        let code = r.i16()?;
        let msg = r.nstring()?;
        let bytes = r.bytes()?;
        if code != 0 {
            return Err(MsgError::Auth(format!("SASL {}: {}", mech.name(), msg.unwrap_or_else(|| KafkaCode(code).name().into()))));
        }
        Ok(bytes)
    };
    match mech {
        Sasl::Plain => {
            *corr += 1;
            let (body, c) = round([&b"\0"[..], user.as_bytes(), b"\0", pass.as_bytes()].concat(), *corr);
            auth(&direct(io, Api::SaslAuthenticate, c, &opts.client_id, &body).await?)?;
        }
        Sasl::ScramSha256 | Sasl::ScramSha512 => {
            use kernway_wire::scram::{new_nonce, Binding, Hash, Scram};
            let hash = if *mech == Sasl::ScramSha256 { Hash::Sha256 } else { Hash::Sha512 };
            let mut s = Scram::new(hash, user, pass.as_bytes(), new_nonce().map_err(net)?, Binding::None);
            *corr += 1;
            let (body, c) = round(s.client_first().into_bytes(), *corr);
            let server_first = auth(&direct(io, Api::SaslAuthenticate, c, &opts.client_id, &body).await?)?;
            let fin = s.client_final(&String::from_utf8_lossy(&server_first)).map_err(MsgError::Auth)?;
            *corr += 1;
            let (body, c) = round(fin.into_bytes(), *corr);
            let server_final = auth(&direct(io, Api::SaslAuthenticate, c, &opts.client_id, &body).await?)?;
            s.verify_server(&String::from_utf8_lossy(&server_final)).map_err(MsgError::Auth)?;
        }
    }
    Ok(())
}

impl BrokerConn {
    /// Connect, check versions, authenticate; the actor takes over.
    pub async fn connect(opts: &Options, host: &str, port: u16) -> Result<Self, MsgError> {
        let addr = format!("{host}:{port}");
        let fut = async {
            let tcp = kernway_wire::offload::connect_tcp(host, port).await.map_err(|e| MsgError::Connection(format!("connect {addr}: {e}")))?;
            let mut io = BufStream::new(tcp);
            if let Some(cfg) = tls_config(opts).await? {
                io.upgrade_tls(cfg, host).await.map_err(|e| MsgError::Connection(format!("TLS with {addr}: {e}")))?;
            }
            let mut corr = 0;
            let mut w = W::new(true);
            w.string("kernway-kafka").string(env!("CARGO_PKG_VERSION")).tags();
            let b = direct(&mut io, Api::ApiVersions, corr, &opts.client_id, &w.b).await?;
            check_versions(&b)?;
            if let Some(sasl) = &opts.sasl {
                authenticate(&mut io, opts, sasl, &mut corr).await?;
            }
            Ok::<_, MsgError>((io, corr + 1))
        };
        let (io, next_corr) = match rt_core::timeout(opts.connect_timeout, fut).await {
            Ok(r) => r?,
            Err(_) => return Err(MsgError::Connection(format!("connect {addr}: timed out"))),
        };
        let inner = Rc::new(Inner {
            client_id: opts.client_id.clone(),
            request_timeout: opts.request_timeout,
            st: RefCell::new(State { next_corr, ..State::default() }),
        });
        rt_core::spawn(actor(Rc::downgrade(&inner), io));
        Ok(BrokerConn { inner, addr })
    }

    /// Whether the connection is up.
    pub fn is_open(&self) -> bool {
        self.inner.st.borrow().closed.is_none()
    }

    /// Send a request and await its response body (the default timeout).
    pub async fn call(&self, api: Api, body: &[u8]) -> Result<Vec<u8>, MsgError> {
        self.call_timeout(api, body, self.inner.request_timeout).await
    }

    /// [`call`](Self::call) with a timeout (long polls: fetch, join).
    pub async fn call_timeout(&self, api: Api, body: &[u8], timeout: Duration) -> Result<Vec<u8>, MsgError> {
        let corr = {
            let mut st = self.inner.st.borrow_mut();
            if let Some(e) = &st.closed {
                return Err(e.clone());
            }
            let corr = st.next_corr;
            st.next_corr = st.next_corr.wrapping_add(1).max(1);
            st.pending.insert(corr, Slot::default());
            let req = request(api, corr, &self.inner.client_id, body);
            st.out.extend_from_slice(&req);
            if let Some(w) = st.writer.take() {
                w.wake();
            }
            corr
        };
        struct Forget<'a>(&'a Inner, i32);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.st.borrow_mut().pending.remove(&self.1);
            }
        }
        let _forget = Forget(&self.inner, corr);
        let inner = self.inner.clone();
        let wait = std::future::poll_fn(move |cx| {
            let mut st = inner.st.borrow_mut();
            let closed = st.closed.clone();
            let Some(slot) = st.pending.get_mut(&corr) else {
                return Poll::Ready(Err(MsgError::Protocol("Kafka: request vanished".into())));
            };
            if let Some(r) = slot.reply.take() {
                return Poll::Ready(r);
            }
            if let Some(e) = closed {
                return Poll::Ready(Err(e));
            }
            slot.waker = Some(cx.waker().clone());
            Poll::Pending
        });
        let frame = match rt_core::timeout(timeout, wait).await {
            Ok(r) => r?,
            Err(_) => {
                // The response may still come and would be matched to nothing;
                // the connection is suspect: close it.
                self.close(MsgError::Connection(format!("{} to {} timed out after {timeout:?}", api.name(), self.addr)));
                return Err(MsgError::Connection(format!("{} to {} timed out after {timeout:?}", api.name(), self.addr)));
            }
        };
        let (_, body) = response_body(api, &frame)?;
        Ok(body.to_vec())
    }

    /// Close (pending calls fail with `e`).
    pub fn close(&self, e: MsgError) {
        let mut st = self.inner.st.borrow_mut();
        st.closed.get_or_insert(e);
        for s in st.pending.values_mut() {
            if let Some(w) = s.waker.take() {
                w.wake();
            }
        }
        if let Some(w) = st.writer.take() {
            w.wake();
        }
    }
}

enum Event {
    Frame(Vec<u8>),
    Write,
    Stop,
}

async fn actor(weak: Weak<Inner>, mut io: BufStream) {
    let result: Result<(), MsgError> = async {
        loop {
            let ev = {
                let Some(inner) = weak.upgrade() else { return Ok(()) };
                let mut read = std::pin::pin!(read_frame(&mut io));
                std::future::poll_fn(|cx| {
                    {
                        let mut st = inner.st.borrow_mut();
                        if st.closed.is_some() {
                            return Poll::Ready(Ok(Event::Stop));
                        }
                        if !st.out.is_empty() {
                            return Poll::Ready(Ok(Event::Write));
                        }
                        st.writer = Some(cx.waker().clone());
                    }
                    match read.as_mut().poll(cx) {
                        Poll::Ready(r) => Poll::Ready(r.map(Event::Frame)),
                        Poll::Pending => Poll::Pending,
                    }
                })
                .await?
            };
            let Some(inner) = weak.upgrade() else { return Ok(()) };
            match ev {
                Event::Stop => return Ok(()),
                Event::Write => {
                    let out = std::mem::take(&mut inner.st.borrow_mut().out);
                    io.write_buf().extend_from_slice(&out);
                    io.flush().await.map_err(net)?;
                    io.write_buf();
                }
                Event::Frame(f) => {
                    if f.len() < 4 {
                        return Err(MsgError::Protocol("Kafka: response without a correlation id".into()));
                    }
                    let corr = i32::from_be_bytes([f[0], f[1], f[2], f[3]]);
                    let mut st = inner.st.borrow_mut();
                    // A response nobody waits for any more (timed out): dropped.
                    if let Some(slot) = st.pending.get_mut(&corr) {
                        slot.reply = Some(Ok(f));
                        if let Some(w) = slot.waker.take() {
                            w.wake();
                        }
                    }
                }
            }
        }
    }
    .await;
    if let Some(inner) = weak.upgrade() {
        let e = result.err().unwrap_or_else(|| MsgError::Connection("connection closed".into()));
        let mut st = inner.st.borrow_mut();
        st.closed.get_or_insert(e);
        for s in st.pending.values_mut() {
            if let Some(w) = s.waker.take() {
                w.wake();
            }
        }
    }
}
