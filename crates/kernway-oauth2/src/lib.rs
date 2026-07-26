//! # kernway-oauth2
//!
//! OAuth2 / OIDC **client** — "log in with Google / GitHub", on the Kernway HTTP
//! client. Implements the Authorization Code flow with **PKCE** (RFC 7636), which is
//! the current best practice even for confidential clients.
//!
//! The flow, and where your app fits:
//!
//! 1. [`authorize_url`](OAuth2Client::authorize_url) → an [`Authorization`]: redirect
//!    the user to its `url`, and **store its `state` + `pkce_verifier`** in their
//!    session.
//! 2. The provider redirects back to your `redirect_uri` with `?code=…&state=…`.
//!    **Check the returned `state` equals the stored one** (CSRF defence), then call
//! 3. [`exchange_code`](OAuth2Client::exchange_code) with the `code` and the stored
//!    verifier → a [`TokenResponse`] (access token, maybe an `id_token`).
//! 4. [`userinfo`](OAuth2Client::userinfo) with the access token → the user's profile,
//!    from which you create your own session.
//!
//! Steps 1 and the state check are pure/local; 3 and 4 make the outbound HTTPS calls.
//!
//! ```rust,ignore
//! let google = OAuth2Client::google(id, secret, "https://app.example/callback");
//! let auth = google.authorize_url();            // redirect to auth.url; save auth.state + verifier
//! // … on the callback, after checking state …
//! let tokens = google.exchange_code(&code, &verifier).await?;
//! let user = google.userinfo(&tokens.access_token).await?;
//! let session = my_login(user.email().unwrap_or_default());
//! ```

#![forbid(unsafe_code)]

use kernway_http_client::{percent_encode, HttpClient, HttpError, Method, Request, Url};
use kernway_security::hash::sha256;
use kernway_security::token::b64url_encode;
use serde::Deserialize;
use serde_json::Value;

/// An OAuth2 client for one provider + one app registration.
pub struct OAuth2Client {
    client_id: String,
    client_secret: String,
    auth_url: String,
    token_url: String,
    userinfo_url: Option<String>,
    redirect_uri: String,
    scopes: Vec<String>,
    http: HttpClient,
}

impl OAuth2Client {
    /// A client for an arbitrary provider. Prefer [`google`](Self::google) /
    /// [`github`](Self::github) for the common ones.
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        auth_url: impl Into<String>,
        token_url: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            auth_url: auth_url.into(),
            token_url: token_url.into(),
            userinfo_url: None,
            redirect_uri: redirect_uri.into(),
            scopes: Vec::new(),
            http: HttpClient::new(),
        }
    }

    /// Google, pre-filled with its OIDC endpoints and `openid email profile` scopes.
    pub fn google(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        let mut c = Self::new(
            client_id,
            client_secret,
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
            redirect_uri,
        );
        c.userinfo_url = Some("https://openidconnect.googleapis.com/v1/userinfo".into());
        c.scopes = vec!["openid".into(), "email".into(), "profile".into()];
        c
    }

    /// GitHub, pre-filled with its endpoints and `read:user user:email` scopes.
    pub fn github(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        let mut c = Self::new(
            client_id,
            client_secret,
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            redirect_uri,
        );
        c.userinfo_url = Some("https://api.github.com/user".into());
        c.scopes = vec!["read:user".into(), "user:email".into()];
        c
    }

    /// Replace the requested scopes.
    #[must_use]
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Set the OIDC/userinfo endpoint (already set for the presets).
    #[must_use]
    pub fn userinfo_url(mut self, url: impl Into<String>) -> Self {
        self.userinfo_url = Some(url.into());
        self
    }

    /// Build the authorization redirect URL, with a fresh PKCE verifier + `state`.
    /// Store the returned `state` and `pkce_verifier` in the user's session — you need
    /// them on the callback.
    #[must_use]
    pub fn authorize_url(&self) -> Authorization {
        let verifier = random_b64url(32); // 43 base64url chars — a valid PKCE verifier
        let challenge = b64url_encode(&sha256(verifier.as_bytes())); // S256
        let state = random_b64url(16);
        let scope = self.scopes.join(" ");

        let params = [
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("scope", scope.as_str()),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={}", percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let sep = if self.auth_url.contains('?') { '&' } else { '?' };

        Authorization { url: format!("{}{sep}{query}", self.auth_url), state, pkce_verifier: verifier }
    }

    /// Exchange the `code` from the callback (with the PKCE `verifier` you stored) for
    /// tokens.
    pub async fn exchange_code(&self, code: &str, verifier: &str) -> Result<TokenResponse, OAuth2Error> {
        let body = form_body(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("code_verifier", verifier),
        ]);
        let req = Request::new(Method::Post, Url::parse(&self.token_url)?)
            .body("application/x-www-form-urlencoded", body)
            // GitHub returns form-encoded unless asked for JSON; Google ignores this.
            .header("accept", "application/json");

        let resp = self.http.send(req).await?;
        if !resp.is_success() {
            return Err(OAuth2Error::Status(resp.status, resp.text()));
        }
        serde_json::from_slice(&resp.body).map_err(|e| OAuth2Error::Parse(e.to_string()))
    }

    /// Fetch the user's profile with an access token (the OIDC `userinfo` endpoint, or
    /// the provider's user API).
    pub async fn userinfo(&self, access_token: &str) -> Result<UserInfo, OAuth2Error> {
        let url = self.userinfo_url.as_deref().ok_or_else(|| OAuth2Error::Config("no userinfo_url set".into()))?;
        let req = Request::new(Method::Get, Url::parse(url)?)
            .header("authorization", format!("Bearer {access_token}"))
            .header("accept", "application/json");

        let resp = self.http.send(req).await?;
        if !resp.is_success() {
            return Err(OAuth2Error::Status(resp.status, resp.text()));
        }
        let raw: Value = serde_json::from_slice(&resp.body).map_err(|e| OAuth2Error::Parse(e.to_string()))?;
        Ok(UserInfo { raw })
    }
}

/// The redirect to send the user to, plus the values to store for the callback.
#[derive(Debug, Clone)]
pub struct Authorization {
    /// The provider URL to redirect the browser to.
    pub url: String,
    /// An opaque anti-CSRF value — must match the `state` on the callback.
    pub state: String,
    /// The PKCE code verifier — pass it to [`exchange_code`](OAuth2Client::exchange_code).
    pub pkce_verifier: String,
}

/// The provider's token response (RFC 6749 §5.1). Extra fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    /// The access token (use it as a `Bearer` credential).
    pub access_token: String,
    /// Usually `"Bearer"`.
    #[serde(default)]
    pub token_type: String,
    /// Lifetime in seconds, if the provider states it.
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// A refresh token, if issued.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// The OIDC ID token (a JWT), if `openid` scope was requested.
    #[serde(default)]
    pub id_token: Option<String>,
    /// The granted scopes.
    #[serde(default)]
    pub scope: Option<String>,
}

/// A user profile — providers disagree on field names, so this exposes the raw JSON
/// plus best-effort accessors that try the common keys.
#[derive(Debug, Clone)]
pub struct UserInfo {
    /// The provider's raw userinfo JSON.
    pub raw: Value,
}

impl UserInfo {
    /// A string field by key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.raw.get(key).and_then(Value::as_str)
    }

    /// The stable subject id — `sub` (OIDC) or `id` (e.g. GitHub).
    #[must_use]
    pub fn subject(&self) -> Option<String> {
        self.get("sub")
            .map(str::to_string)
            .or_else(|| self.raw.get("id").map(|v| v.to_string().trim_matches('"').to_string()))
    }

    /// The email, if present and shared.
    #[must_use]
    pub fn email(&self) -> Option<&str> {
        self.get("email")
    }

    /// The display name — `name` (OIDC) or `login` (GitHub).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.get("name").or_else(|| self.get("login"))
    }
}

/// An OAuth2 flow failure.
#[derive(Debug)]
pub enum OAuth2Error {
    /// The underlying HTTP call failed.
    Http(HttpError),
    /// The provider returned a non-2xx status (with its body).
    Status(u16, String),
    /// The response JSON could not be parsed.
    Parse(String),
    /// The client is misconfigured (e.g. no userinfo endpoint).
    Config(String),
}

impl std::fmt::Display for OAuth2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuth2Error::Http(e) => write!(f, "oauth2 http error: {e}"),
            OAuth2Error::Status(code, body) => write!(f, "oauth2 provider returned {code}: {body}"),
            OAuth2Error::Parse(m) => write!(f, "oauth2 parse error: {m}"),
            OAuth2Error::Config(m) => write!(f, "oauth2 config error: {m}"),
        }
    }
}

impl std::error::Error for OAuth2Error {}

impl From<HttpError> for OAuth2Error {
    fn from(e: HttpError) -> Self {
        OAuth2Error::Http(e)
    }
}

/// `n` random bytes as base64url — a PKCE verifier (n=32 → 43 chars) or a `state`.
fn random_b64url(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes).expect("OS randomness unavailable");
    b64url_encode(&bytes)
}

/// `application/x-www-form-urlencoded` from key/value pairs.
fn form_body(pairs: &[(&str, &str)]) -> Vec<u8> {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_to(token_url: &str, userinfo_url: &str) -> OAuth2Client {
        let mut c = OAuth2Client::new("client-id", "secret", "https://accounts.example/auth", token_url, "https://app.example/cb");
        c.userinfo_url = Some(userinfo_url.to_string());
        c.scopes = vec!["openid".into(), "email".into()];
        c
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let google = OAuth2Client::google("my-client", "my-secret", "https://app.example/callback");
        let auth = google.authorize_url();

        let parsed = Url::parse(&auth.url).unwrap();
        assert_eq!(parsed.host, "accounts.google.com");
        let q = parsed.path_and_query;
        assert!(q.contains("response_type=code"));
        assert!(q.contains("client_id=my-client"));
        assert!(q.contains("code_challenge_method=S256"));
        assert!(q.contains(&format!("state={}", auth.state)));
        // scope "openid email profile" → percent-encoded spaces.
        assert!(q.contains("scope=openid%20email%20profile"), "scopes space-encoded: {q}");

        // The challenge must be exactly base64url(sha256(verifier)).
        let expected = b64url_encode(&sha256(auth.pkce_verifier.as_bytes()));
        assert!(q.contains(&format!("code_challenge={expected}")), "PKCE S256 challenge derived from verifier");
        // The verifier is a valid length (43 chars for 32 bytes).
        assert_eq!(auth.pkce_verifier.len(), 43);
    }

    #[test]
    fn two_authorizations_differ() {
        let g = OAuth2Client::google("c", "s", "https://app/cb");
        let a = g.authorize_url();
        let b = g.authorize_url();
        assert_ne!(a.state, b.state, "state is random per request");
        assert_ne!(a.pkce_verifier, b.pkce_verifier, "verifier is random per request");
    }

    /// A one-shot local HTTP server that replies to the next connection with `body` as
    /// a JSON 200. Returns its `http://127.0.0.1:port`.
    fn mock_json(body: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = [0u8; 2048];
            let n = sock.read(&mut req).unwrap();
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            sock.write_all(resp.as_bytes()).unwrap();
            req[..n].to_vec() // hand the received request back for assertions
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    #[test]
    fn exchange_code_posts_the_form_and_parses_tokens() {
        let (token_url, server) = mock_json(r#"{"access_token":"ya29.abc","token_type":"Bearer","expires_in":3599,"id_token":"eyJ..."}"#);
        let client = client_to(&token_url, "http://unused");

        let tokens = block(client.exchange_code("auth-code-xyz", "the-verifier")).unwrap();
        assert_eq!(tokens.access_token, "ya29.abc");
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_in, Some(3599));
        assert!(tokens.id_token.is_some());

        // The request we sent carried the code, the verifier, and the secret as a form.
        let sent = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(sent.starts_with("POST "));
        assert!(sent.contains("grant_type=authorization_code"));
        assert!(sent.contains("code=auth-code-xyz"));
        assert!(sent.contains("code_verifier=the-verifier"));
        assert!(sent.contains("client_secret=secret"));
        assert!(sent.contains("accept: application/json"), "asks for JSON (GitHub needs it)");
    }

    #[test]
    fn userinfo_reads_the_profile_across_providers() {
        // OIDC shape (Google).
        let (u, server) = mock_json(r#"{"sub":"108","email":"alice@example.com","name":"Alice"}"#);
        let client = client_to("http://unused", &u);
        let info = block(client.userinfo("access-tok")).unwrap();
        assert_eq!(info.subject().as_deref(), Some("108"));
        assert_eq!(info.email(), Some("alice@example.com"));
        assert_eq!(info.name(), Some("Alice"));
        let sent = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(sent.contains("authorization: Bearer access-tok"), "sent the bearer token");

        // GitHub shape: id + login, no sub/name.
        let (u2, s2) = mock_json(r#"{"id":42,"login":"octocat"}"#);
        let gh = client_to("http://unused", &u2);
        let info = block(gh.userinfo("t")).unwrap();
        assert_eq!(info.subject().as_deref(), Some("42"), "falls back to id");
        assert_eq!(info.name(), Some("octocat"), "falls back to login");
        s2.join().unwrap();
    }
}
