//! A cookie jar — collect `Set-Cookie` from responses and send matching `Cookie`
//! headers on later requests. Opt-in ([`HttpClient::cookie_store`](crate::HttpClient::cookie_store)),
//! since API clients using bearer tokens usually do not want implicit cookies.
//!
//! Deliberately small: name/value, `Domain`, `Path`, `Max-Age`, and `Secure` — enough
//! for login/session flows. `Expires` (a date) is treated as a session cookie rather
//! than parsing HTTP dates; `HttpOnly`/`SameSite` are irrelevant to a programmatic
//! client and ignored.

use std::sync::Mutex;

use crate::Url;

/// One stored cookie.
struct Cookie {
    name: String,
    value: String,
    /// The domain it applies to (lowercased, no leading dot).
    domain: String,
    /// `true` when there was no `Domain` attribute — matches the exact host only.
    host_only: bool,
    /// The path prefix it applies to.
    path: String,
    /// Expiry in unix seconds, or `None` for a session cookie.
    expires: Option<u64>,
    /// Only send over `https`.
    secure: bool,
}

/// A jar of cookies, shared across requests (and cores) behind a `Mutex`.
pub(crate) struct CookieJar {
    cookies: Mutex<Vec<Cookie>>,
}

impl CookieJar {
    pub(crate) fn new() -> Self {
        Self { cookies: Mutex::new(Vec::new()) }
    }

    /// Store the `Set-Cookie` values from a response received from `url`.
    pub(crate) fn store<'a>(&self, set_cookies: impl Iterator<Item = &'a str>, url: &Url, now: u64) {
        let mut jar = self.cookies.lock().unwrap();
        for raw in set_cookies {
            let Some(cookie) = parse_set_cookie(raw, url, now) else {
                continue;
            };
            // A cookie is identified by (name, domain, path); a new one replaces it.
            jar.retain(|c| !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path));
            // An already-expired cookie is a deletion — do not re-add it.
            if cookie.expires.is_none_or(|e| e > now) {
                jar.push(cookie);
            }
        }
    }

    /// The `Cookie` header value for a request to `url` (empty if nothing matches).
    pub(crate) fn header_for(&self, url: &Url, now: u64) -> String {
        let host = url.host.to_ascii_lowercase();
        let jar = self.cookies.lock().unwrap();
        jar.iter()
            .filter(|c| c.expires.is_none_or(|e| e > now))
            .filter(|c| !c.secure || url.scheme == "https")
            .filter(|c| domain_match(&c.domain, c.host_only, &host))
            .filter(|c| path_match(&c.path, &url.path_and_query))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Parse a `Set-Cookie` value against the response URL.
fn parse_set_cookie(raw: &str, url: &Url, now: u64) -> Option<Cookie> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim().to_string();
    let value = value.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut cookie = Cookie {
        name,
        value,
        domain: url.host.to_ascii_lowercase(),
        host_only: true,
        path: default_path(&url.path_and_query),
        expires: None,
        secure: false,
    };

    for attr in parts {
        let attr = attr.trim();
        let (key, val) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (attr.to_ascii_lowercase(), ""),
        };
        match key.as_str() {
            "domain" => {
                let d = val.trim_start_matches('.').to_ascii_lowercase();
                if !d.is_empty() {
                    cookie.domain = d;
                    cookie.host_only = false;
                }
            }
            "path" if val.starts_with('/') => cookie.path = val.to_string(),
            "max-age" => {
                if let Ok(secs) = val.parse::<i64>() {
                    cookie.expires = Some(if secs <= 0 { 0 } else { now.saturating_add(secs as u64) });
                }
            }
            "secure" => cookie.secure = true,
            _ => {} // expires (a date), httponly, samesite — ignored
        }
    }
    Some(cookie)
}

/// The default path of a cookie: the directory of the request path.
fn default_path(path_and_query: &str) -> String {
    let path = path_and_query.split('?').next().unwrap_or("/");
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// Whether `host` matches the cookie's domain (exact, or a subdomain if `Domain` was set).
fn domain_match(domain: &str, host_only: bool, host: &str) -> bool {
    if host_only {
        host == domain
    } else {
        host == domain || host.ends_with(&format!(".{domain}"))
    }
}

/// Whether the request path is within the cookie's path (RFC 6265 §5.1.4).
fn path_match(cookie_path: &str, request_path: &str) -> bool {
    let path = request_path.split('?').next().unwrap_or("/");
    path == cookie_path
        || (path.starts_with(cookie_path)
            && (cookie_path.ends_with('/') || path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn stores_and_sends_a_session_cookie() {
        let jar = CookieJar::new();
        jar.store(["sid=abc; Path=/"].into_iter(), &url("https://x.example/login"), 1000);
        assert_eq!(jar.header_for(&url("https://x.example/dashboard"), 1000), "sid=abc");
    }

    #[test]
    fn honours_path_domain_secure_and_expiry() {
        let jar = CookieJar::new();
        jar.store(
            [
                "a=1; Path=/admin",           // path-scoped
                "b=2; Secure",                // https only
                "c=3; Max-Age=100",           // expires at 1100
                "d=4; Domain=example.com",    // subdomain-matching
            ]
            .into_iter(),
            &url("https://app.example.com/admin/panel"),
            1000,
        );

        // Under /admin over https, before expiry: all match (a is path-scoped to /admin).
        let here = jar.header_for(&url("https://app.example.com/admin/panel"), 1050);
        assert!(here.contains("a=1") && here.contains("b=2") && here.contains("c=3") && here.contains("d=4"), "{here}");

        // A different path drops the /admin cookie.
        assert!(!jar.header_for(&url("https://app.example.com/other"), 1050).contains("a=1"));
        // Plain http drops the Secure cookie.
        assert!(!jar.header_for(&url("http://app.example.com/admin"), 1050).contains("b=2"));
        // After expiry the Max-Age cookie is gone.
        assert!(!jar.header_for(&url("https://app.example.com/admin"), 2000).contains("c=3"));
        // The Domain cookie reaches a sibling subdomain; a host-only would not.
        assert!(jar.header_for(&url("https://api.example.com/admin"), 1050).contains("d=4"));
    }

    #[test]
    fn a_new_value_replaces_and_max_age_zero_deletes() {
        let jar = CookieJar::new();
        let site = url("https://x.example/");
        jar.store(["k=old"].into_iter(), &site, 1000);
        jar.store(["k=new"].into_iter(), &site, 1000);
        assert_eq!(jar.header_for(&site, 1000), "k=new", "replaced");
        jar.store(["k=x; Max-Age=0"].into_iter(), &site, 1000);
        assert_eq!(jar.header_for(&site, 1000), "", "Max-Age=0 deletes it");
    }
}
