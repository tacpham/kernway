//! The HTTP client against real servers — go-httpbin (plain and TLS) and Caddy
//! (mutual TLS), set up by `tests/support/httpbin-setup.sh`, which prints
//! `KERNWAY_HTTPBIN_*`. Without them each test reports `skip:`, which the CI job
//! and the acceptance run treat as a failure.

use kernway_http_client::{HttpClient, HttpError, Method, Request, Url};
use serde_json::Value;
use std::time::{Duration, Instant};

struct Live {
    url: String,
    files_url: String,
    tls_url: String,
    mtls_url: String,
    certs: String,
}

fn live() -> Option<Live> {
    let Ok(url) = std::env::var("KERNWAY_HTTPBIN_URL") else {
        eprintln!("skip: KERNWAY_HTTPBIN_URL not set (tests/support/httpbin-setup.sh)");
        return None;
    };
    let v = |k: &str| std::env::var(k).unwrap();
    Some(Live { url, files_url: v("KERNWAY_HTTPBIN_FILES_URL"), tls_url: v("KERNWAY_HTTPBIN_TLS_URL"), mtls_url: v("KERNWAY_HTTPBIN_MTLS_URL"), certs: v("KERNWAY_HTTPBIN_CERTS") })
}

impl Live {
    fn pem(&self, name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/{name}", self.certs)).unwrap()
    }
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn json(r: &kernway_http_client::Response) -> Value {
    serde_json::from_slice(&r.body).unwrap_or_else(|e| panic!("{e}: {}", r.text()))
}

fn req(m: Method, url: &str) -> Request {
    Request::new(m, Url::parse(url).unwrap())
}

#[test]
fn requests_carry_their_headers_query_and_body() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        let r = c
            .send(req(Method::Get, &format!("{}/get?q=t%C3%ACm&q=2&x=a b", l.url)).header("X-Kernway", "xin chào"))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let j = json(&r);
        assert_eq!(j["args"]["q"], serde_json::json!(["tìm", "2"]));
        assert_eq!(j["args"]["x"], serde_json::json!(["a b"]), "a raw space is sent percent-encoded");
        assert_eq!(j["headers"]["X-Kernway"][0], "xin chào", "UTF-8 header values reach the server intact");
        assert!(j["headers"]["User-Agent"][0].as_str().unwrap().starts_with("kernway-http-client/"));

        for m in [Method::Post, Method::Put, Method::Patch, Method::Delete] {
            let body = format!("{{\"m\":\"{}\",\"vi\":\"Phở bò\"}}", m.as_str());
            let r = c.send(req(m, &format!("{}/anything/x", l.url)).body("application/json", body.clone())).await.unwrap();
            let j = json(&r);
            assert_eq!(j["method"], m.as_str());
            assert_eq!(j["data"], body, "{} body byte-exact", m.as_str());
            assert_eq!(j["json"]["vi"], "Phở bò");
        }
        let r = c.send(req(Method::Post, &format!("{}/anything", l.url))).await.unwrap();
        assert_eq!(json(&r)["headers"]["Content-Length"][0], "0", "a bodiless POST says so");

        let r = c.post_form(&format!("{}/post", l.url), &[("name", "Nguyễn Văn A"), ("note", "a&b=c d")]).await.unwrap();
        let j = json(&r);
        assert_eq!(j["form"]["name"][0], "Nguyễn Văn A");
        assert_eq!(j["form"]["note"][0], "a&b=c d");

        let r = c.send(req(Method::Options, &format!("{}/get", l.url))).await.unwrap();
        assert!(r.status < 300, "OPTIONS answered {}", r.status);
        let head = c.send(req(Method::Head, &format!("{}/bytes/1000", l.url))).await.unwrap();
        assert_eq!((head.status, head.body.len()), (200, 0), "HEAD has no body whatever its Content-Length");
        let after = c.get(&format!("{}/bytes/7", l.url)).await.unwrap();
        assert_eq!(after.body.len(), 7, "the connection is still in step after the HEAD");
    });
}

#[test]
fn status_codes_and_bodiless_responses() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        for code in [200u16, 201, 204, 304, 400, 401, 404, 418, 500, 503] {
            let r = c.get(&format!("{}/status/{code}", l.url)).await.unwrap();
            assert_eq!(r.status, code);
            assert_eq!(r.is_success(), (200..300).contains(&code));
            if code == 204 || code == 304 {
                assert!(r.body.is_empty());
            }
        }
        // Repeated headers are all kept, in order.
        let r = c.get(&format!("{}/response-headers?X-Multi=1&X-Multi=2", l.url)).await.unwrap();
        let multi: Vec<&str> = r.headers().filter(|(k, _)| k.eq_ignore_ascii_case("x-multi")).map(|(_, v)| v).collect();
        assert_eq!(multi, ["1", "2"]);
    });
}

#[test]
fn redirects_are_followed_with_the_method_rules() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        for path in ["redirect/3", "relative-redirect/3", "absolute-redirect/3"] {
            let r = c.get(&format!("{}/{path}", l.url)).await.unwrap();
            assert_eq!(r.status, 200, "{path}");
            assert!(json(&r)["url"].as_str().unwrap().ends_with("/get"), "{path}");
        }
        let r = HttpClient::new().max_redirects(0).get(&format!("{}/redirect/1", l.url)).await.unwrap();
        assert_eq!(r.status, 302);
        let r = HttpClient::new().max_redirects(2).get(&format!("{}/redirect/5", l.url)).await.unwrap();
        assert_eq!(r.status, 302, "the budget ran out: the last 3xx is returned");

        // 307 / 308 keep the method and body; 303 becomes a GET.
        for (code, method) in [(307, "POST"), (308, "POST"), (303, "GET"), (302, "GET")] {
            let url = format!("{}/redirect-to?url=/anything&status_code={code}", l.url);
            let r = c.send(req(Method::Post, &url).body("text/plain", "payload")).await.unwrap();
            let j = json(&r);
            assert_eq!(j["method"], method, "{code}");
            assert_eq!(j["data"], if method == "POST" { "payload" } else { "" }, "{code}");
        }
    });
}

#[test]
fn credentials_never_follow_a_redirect_to_another_origin() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        // 127.0.0.1 → localhost is another origin, even on the same server.
        let other = l.url.replace("127.0.0.1", "localhost");
        let url = format!("{}/redirect-to?url={}/headers", l.url, kernway_http_client::percent_encode(&other));
        let r = c
            .send(req(Method::Get, &url).bearer_auth("secret").header("Cookie", "sid=1").header("Proxy-Authorization", "Basic eA=="))
            .await
            .unwrap();
        let h = &json(&r)["headers"];
        assert!(h.get("Authorization").is_none() && h.get("Cookie").is_none() && h.get("Proxy-Authorization").is_none(), "{h}");
        // Same origin: they stay.
        let r = c.send(req(Method::Get, &format!("{}/redirect-to?url=/headers", l.url)).bearer_auth("secret")).await.unwrap();
        assert_eq!(json(&r)["headers"]["Authorization"][0], "Bearer secret");
    });
}

#[test]
fn basic_and_bearer_authentication() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        let url = format!("{}/basic-auth/kw/p%C3%A1ss", l.url);
        assert_eq!(c.send(req(Method::Get, &url).basic_auth("kw", "páss")).await.unwrap().status, 200);
        assert_eq!(c.send(req(Method::Get, &url).basic_auth("kw", "wrong")).await.unwrap().status, 401);
        assert_eq!(c.send(req(Method::Get, &url)).await.unwrap().status, 401);
        let with_creds = l.url.replacen("://", "://kw:p%C3%A1ss@", 1);
        assert_eq!(c.get(&format!("{with_creds}/basic-auth/kw/p%C3%A1ss")).await.unwrap().status, 200, "user:password@ in the URL");
        assert_eq!(c.send(req(Method::Get, &format!("{}/bearer", l.url)).bearer_auth("tok")).await.unwrap().status, 200);
        assert_eq!(c.get(&format!("{}/bearer", l.url)).await.unwrap().status, 401);
    });
}

#[test]
fn cookies_are_stored_sent_and_expired() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new().cookie_store(true);
        let r = c.get(&format!("{}/cookies/set?a=1&b=x%20y", l.url)).await.unwrap();
        assert_eq!(json(&r)["cookies"], serde_json::json!({"a": "1", "b": "x y"}), "set on the 302, sent on the follow-up");
        let r = c.get(&format!("{}/cookies/delete?a", l.url)).await.unwrap();
        assert_eq!(json(&r)["cookies"], serde_json::json!({"b": "x y"}), "an expired cookie is dropped");
        let fresh = HttpClient::new();
        assert_eq!(json(&fresh.get(&format!("{}/cookies", l.url)).await.unwrap())["cookies"], serde_json::json!({}), "off by default");
    });
}

#[test]
fn compressed_responses_are_inflated() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        for (path, flag) in [("gzip", "gzipped"), ("deflate", "deflated")] {
            let r = c.get(&format!("{}/{path}", l.url)).await.unwrap();
            assert_eq!(json(&r)[flag], true, "{path}");
            assert!(r.header("content-encoding").is_none(), "{path}: the header no longer describes the body");
        }
        // Brotli: Caddy serves a precompressed .br next to the file it stands for.
        let www = format!("{}/www", l.certs);
        let text = "Tiệm nail — bảng giá dịch vụ\n".repeat(400);
        std::fs::write(format!("{www}/menu.txt"), &text).unwrap();
        std::fs::write(format!("{www}/menu.txt.br"), kernway_compress::encode(text.as_bytes(), kernway_compress::Encoding::Br)).unwrap();
        let mut raw = c.send_streaming(req(Method::Get, &format!("{}/menu.txt", l.files_url)).header("Accept-Encoding", "br")).await.unwrap();
        assert_eq!(raw.header("content-encoding"), Some("br"), "the server really sent brotli");
        while raw.chunk().await.unwrap().is_some() {}
        let r = c.get(&format!("{}/menu.txt", l.files_url)).await.unwrap();
        assert_eq!(r.text(), text, "inflated byte for byte");
        // The cap applies to the inflated size.
        let e = HttpClient::new().max_response_size(100).get(&format!("{}/gzip", l.url)).await.unwrap_err();
        assert!(matches!(e, HttpError::TooLarge(_)), "{e}");
    });
}

#[test]
fn chunked_bodies_trailers_ranges_and_streaming() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        let r = c.get(&format!("{}/trailers?X-Checksum=abc123&X-Count=2", l.url)).await.unwrap();
        assert_eq!(r.trailer("X-Checksum"), Some("abc123"));
        assert_eq!(r.trailer("x-count"), Some("2"));
        assert_eq!(json(&r)["url"].as_str().map(|u| u.contains("/trailers")), Some(true));

        let r = c.get(&format!("{}/stream/20", l.url)).await.unwrap();
        assert_eq!(r.text().lines().count(), 20, "a chunked body of 20 JSON lines");

        let r = c.get(&format!("{}/bytes/300000?seed=7", l.url)).await.unwrap();
        assert_eq!(r.body.len(), 300_000);
        assert_eq!(c.get(&format!("{}/bytes/300000?seed=7", l.url)).await.unwrap().body, r.body, "the same bytes again");

        let r = c.send(req(Method::Get, &format!("{}/range/100", l.url)).header("Range", "bytes=10-19")).await.unwrap();
        assert_eq!(r.status, 206);
        assert_eq!(r.text(), "klmnopqrst");

        let e = HttpClient::new().max_response_size(1000).get(&format!("{}/bytes/5000", l.url)).await.unwrap_err();
        assert!(matches!(e, HttpError::TooLarge(_)), "{e}");

        // Streamed: the body arrives a piece at a time, and nothing caps it.
        let mut s = HttpClient::new().max_response_size(10).get_streaming(&format!("{}/stream-bytes/200000?chunk_size=1000", l.url)).await.unwrap();
        let (mut total, mut pieces) = (0, 0);
        while let Some(p) = s.chunk().await.unwrap() {
            total += p.len();
            pieces += 1;
        }
        assert_eq!(total, 200_000);
        assert!(pieces > 1);

        // Server-sent events, as they are sent.
        let mut s = c.get_streaming(&format!("{}/sse?count=3&delay=200ms&duration=1s", l.url)).await.unwrap();
        assert!(s.header("content-type").unwrap().starts_with("text/event-stream"));
        let started = Instant::now();
        let mut first_at = None;
        let mut text = String::new();
        while let Some(p) = s.chunk().await.unwrap() {
            first_at.get_or_insert_with(|| started.elapsed());
            text.push_str(&String::from_utf8_lossy(&p));
        }
        assert_eq!(text.matches("event: ping").count(), 3, "{text}");
        assert!(first_at.unwrap() < started.elapsed(), "events arrive before the stream ends");
    });
}

#[test]
fn slow_servers_hit_the_timeout_and_the_client_recovers() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new().timeout(Duration::from_millis(800));
        let started = Instant::now();
        let e = c.get(&format!("{}/delay/3", l.url)).await.unwrap_err();
        assert!(matches!(e, HttpError::Timeout), "{e}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(c.get(&format!("{}/get", l.url)).await.unwrap().status, 200, "usable afterwards");
        // A drip that keeps sending stays within a streaming idle timeout.
        let mut s = c.get_streaming(&format!("{}/drip?numbytes=4&duration=1.2&delay=0", l.url)).await.unwrap();
        let mut n = 0;
        while let Some(p) = s.chunk().await.unwrap() {
            n += p.len();
        }
        assert_eq!(n, 4);
    });
}

#[test]
fn ipv6_and_names_resolve_and_connect() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        let port = l.url.rsplit(':').next().unwrap();
        let r = c.get(&format!("http://[::1]:{port}/headers")).await.unwrap();
        assert_eq!(json(&r)["headers"]["Host"][0], format!("[::1]:{port}"));
        let r = c.get(&format!("http://localhost:{port}/get")).await.unwrap();
        assert_eq!(r.status, 200);
    });
}

#[test]
fn tls_against_a_private_ca() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new().trust_ca_pem(&l.pem("ca.pem")).unwrap();
        let r = c.get(&format!("{}/get", l.tls_url)).await.unwrap();
        assert_eq!(json(&r)["url"], format!("{}/get", l.tls_url));
        let port = l.tls_url.rsplit(':').next().unwrap();
        assert_eq!(c.get(&format!("https://127.0.0.1:{port}/get")).await.unwrap().status, 200, "an IP SAN");
        assert_eq!(c.get(&format!("https://[::1]:{port}/get")).await.unwrap().status, 200, "an IPv6 SAN");
        // Redirects and compression work the same over TLS.
        assert_eq!(json(&c.get(&format!("{}/redirect/2", l.tls_url)).await.unwrap())["url"], format!("{}/get", l.tls_url));
        assert_eq!(json(&c.get(&format!("{}/gzip", l.tls_url)).await.unwrap())["gzipped"], true);

        // Mozilla's roots do not know the test CA; another CA is not this one.
        let e = HttpClient::new().get(&format!("{}/get", l.tls_url)).await.unwrap_err();
        assert!(matches!(&e, HttpError::Tls(m) if m.to_lowercase().contains("unknown")), "{e}");
        let e = HttpClient::new().trust_ca_pem(&l.pem("other-ca.pem")).unwrap().get(&format!("{}/get", l.tls_url)).await.unwrap_err();
        assert!(matches!(e, HttpError::Tls(_)), "{e}");
        assert!(HttpClient::new().trust_ca_pem(b"not a pem").is_err());
    });
}

#[test]
fn mutual_tls_presents_the_client_certificate() {
    let Some(l) = live() else { return };
    block(async move {
        let ca = l.pem("ca.pem");
        let c = HttpClient::new().client_identity_pem(Some(&ca), &l.pem("client.crt"), &l.pem("client.key")).unwrap();
        let r = c.get(&format!("{}/", l.mtls_url)).await.unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "client CN=kw-client"));

        // TLS 1.3 refuses a client certificate after the handshake, with an alert
        // and a close. On Windows the close can arrive as a reset that discards the
        // alert, so there the refusal may show as the connection being reset.
        let refused = |e: &HttpError| {
            matches!(e, HttpError::Tls(_))
                || cfg!(windows) && matches!(e, HttpError::Io(io) if io.kind() == std::io::ErrorKind::ConnectionReset)
        };
        let without = HttpClient::new().trust_ca_pem(&ca).unwrap();
        let e = without.get(&format!("{}/", l.mtls_url)).await.unwrap_err();
        assert!(refused(&e), "no certificate: {e}");
        let intruder = HttpClient::new().client_identity_pem(Some(&ca), &l.pem("intruder.crt"), &l.pem("intruder.key")).unwrap();
        let e = intruder.get(&format!("{}/", l.mtls_url)).await.unwrap_err();
        assert!(refused(&e), "a certificate from another CA: {e}");
        assert!(HttpClient::new().client_identity_pem(None, &l.pem("client.crt"), &l.pem("intruder.key")).is_err(), "a key that is not the certificate's");
    });
}

#[test]
fn a_keep_alive_the_server_closed_is_not_used_for_the_next_post() {
    let Some(l) = live() else { return };
    block(async move {
        // go-httpbin closes idle connections after its 5 s read timeout. The POST after
        // that must go out once, on a new connection, and succeed — not fail on the
        // dead one, and not be sent twice.
        let c = HttpClient::new();
        for tls in [false, true] {
            let (base, c) = if tls {
                (l.tls_url.clone(), c.clone().trust_ca_pem(&l.pem("ca.pem")).unwrap())
            } else {
                (l.url.clone(), c.clone())
            };
            assert_eq!(c.get(&format!("{base}/get")).await.unwrap().status, 200);
            rt_core::sleep(Duration::from_secs(6)).await;
            let r = c.send(req(Method::Post, &format!("{base}/post")).body("text/plain", "once")).await.unwrap();
            assert_eq!(json(&r)["data"], "once", "tls={tls}");
        }
    });
}

#[test]
fn many_requests_share_the_pool_across_tasks() {
    let Some(l) = live() else { return };
    block(async move {
        let c = HttpClient::new();
        let (tx, mut rx) = rt_core::mailbox::<u32>(16);
        for t in 0..8u32 {
            let (c, url, tx) = (c.clone(), l.url.clone(), tx.clone());
            rt_core::spawn(async move {
                for i in 0..25 {
                    let r = c.get(&format!("{url}/anything?t={t}&i={i}")).await.unwrap();
                    assert_eq!(json(&r)["args"]["i"][0], i.to_string());
                }
                tx.try_send(t).unwrap();
            });
        }
        drop(tx);
        let mut done = Vec::new();
        while let Some(t) = rx.recv().await {
            done.push(t);
        }
        done.sort_unstable();
        assert_eq!(done, (0..8).collect::<Vec<_>>(), "every task finished its 25 requests");
    });
}
