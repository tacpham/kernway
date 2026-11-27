//! The S3 client: buckets, objects (with ranges and metadata), listing,
//! copies, multipart uploads and presigned URLs, over SigV4-signed HTTP.

use crate::sigv4::{self, Credentials, EMPTY_SHA256};
use crate::xml;
use kernway_http_client::{HttpClient, Method, Request, Response, Url};
use std::time::Duration;

/// Why an S3 call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3Error {
    /// Bad configuration (endpoint, names).
    Config(String),
    /// The connection failed (refused, timed out, TLS).
    Connection(String),
    /// The credentials were refused (403: signature, key, permissions).
    Denied {
        /// S3's error code (`SignatureDoesNotMatch`, `AccessDenied` …).
        code: String,
        /// Its message.
        message: String,
    },
    /// The bucket or key does not exist (404).
    NotFound {
        /// S3's error code (`NoSuchKey`, `NoSuchBucket` …).
        code: String,
        /// Its message.
        message: String,
    },
    /// Any other S3 error.
    Service {
        /// HTTP status.
        status: u16,
        /// S3's error code.
        code: String,
        /// Its message.
        message: String,
    },
    /// An answer that could not be read.
    Protocol(String),
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            S3Error::Config(m) => write!(f, "S3 configuration: {m}"),
            S3Error::Connection(m) => write!(f, "S3 connection: {m}"),
            S3Error::Denied { code, message } => write!(f, "S3 access denied: {code}: {message}"),
            S3Error::NotFound { code, message } => write!(f, "S3 not found: {code}: {message}"),
            S3Error::Service { status, code, message } => write!(f, "S3 error {status} {code}: {message}"),
            S3Error::Protocol(m) => write!(f, "S3 protocol: {m}"),
        }
    }
}

impl std::error::Error for S3Error {}

/// `Result` with [`S3Error`].
pub type S3Result<T> = Result<T, S3Error>;

/// Where and how to reach the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Config {
    /// `http(s)://host[:port]` — e.g. `https://s3.us-east-1.amazonaws.com`,
    /// `http://127.0.0.1:9000` (MinIO), `https://<account>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    /// Credentials and region.
    pub credentials: Credentials,
    /// Path-style addressing (`endpoint/bucket/key`, MinIO and most
    /// compatible stores) or virtual-hosted (`bucket.endpoint/key`, AWS).
    pub path_style: bool,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl S3Config {
    /// Path-style config (MinIO, R2, B2 …).
    pub fn new(endpoint: impl Into<String>, access_key: impl Into<String>, secret_key: impl Into<String>, region: impl Into<String>) -> Self {
        S3Config {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            credentials: Credentials { access_key: access_key.into(), secret_key: secret_key.into(), session_token: None, region: region.into() },
            path_style: true,
            timeout: Duration::from_secs(60),
        }
    }
}

/// An object read back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Object {
    /// Its bytes (the requested range, for ranged reads).
    pub body: Vec<u8>,
    /// Content type.
    pub content_type: Option<String>,
    /// Entity tag (quotes removed).
    pub etag: String,
    /// Total size of the object (not of a range).
    pub size: u64,
    /// `x-amz-meta-*` metadata (names without the prefix, lowercase).
    pub metadata: Vec<(String, String)>,
}

/// An object as listed or headed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectInfo {
    /// Key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// Entity tag (quotes removed).
    pub etag: String,
    /// Last modified (as the store wrote it).
    pub last_modified: String,
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListPage {
    /// Objects on this page.
    pub objects: Vec<ObjectInfo>,
    /// "Directories" (with a delimiter).
    pub common_prefixes: Vec<String>,
    /// Token for the next page, if any.
    pub next: Option<String>,
}

/// Options for a write.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PutOptions {
    /// Content type.
    pub content_type: Option<String>,
    /// `x-amz-meta-*` metadata (names without the prefix).
    pub metadata: Vec<(String, String)>,
    /// Cache-Control.
    pub cache_control: Option<String>,
}

/// The S3 client. Cheap to clone.
#[derive(Clone)]
pub struct S3Client {
    cfg: S3Config,
    http: HttpClient,
    scheme: String,
    host: String,
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').to_string()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Bucket names S3 accepts (3–63 of `a-z 0-9 . -`, no leading / trailing punctuation).
fn check_bucket(b: &str) -> S3Result<()> {
    let ok = (3..=63).contains(&b.len())
        && b.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
        && b.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && b.bytes().last().is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(S3Error::Config(format!("invalid bucket name {b:?}")))
    }
}

impl S3Client {
    /// A client for `cfg`.
    pub fn new(cfg: S3Config) -> S3Result<Self> {
        let url = Url::parse(&cfg.endpoint).map_err(|e| S3Error::Config(e.to_string()))?;
        let default_port = if url.is_tls() { 443 } else { 80 };
        let host = if url.port == default_port { url.host.clone() } else { format!("{}:{}", url.host, url.port) };
        let http = HttpClient::new().timeout(cfg.timeout).connect_timeout(Duration::from_secs(10)).max_redirects(0);
        Ok(S3Client { scheme: url.scheme.clone(), host, http, cfg })
    }

    /// The configuration.
    pub fn config(&self) -> &S3Config {
        &self.cfg
    }

    /// (host header, path) for a bucket / key.
    fn locate(&self, bucket: &str, key: &str) -> (String, String) {
        let key = sigv4::uri_encode(key, true);
        if bucket.is_empty() {
            return (self.host.clone(), "/".into());
        }
        if self.cfg.path_style {
            (self.host.clone(), if key.is_empty() { format!("/{bucket}") } else { format!("/{bucket}/{key}") })
        } else {
            (format!("{bucket}.{}", self.host), format!("/{key}"))
        }
    }

    /// Send a signed request.
    async fn call(&self, method: Method, bucket: &str, key: &str, query: &[(String, String)], headers: &[(String, String)], body: Vec<u8>) -> S3Result<Response> {
        if !bucket.is_empty() {
            check_bucket(bucket)?;
        }
        let (host, path) = self.locate(bucket, key);
        let date = sigv4::amz_date(now_unix());
        let payload = if body.is_empty() { EMPTY_SHA256.to_string() } else { sigv4::sha256_hex(&body) };
        let mut signed: Vec<(String, String)> = vec![("host".into(), host.clone()), ("x-amz-date".into(), date.clone()), ("x-amz-content-sha256".into(), payload.clone())];
        if let Some(t) = &self.cfg.credentials.session_token {
            signed.push(("x-amz-security-token".into(), t.clone()));
        }
        // The caller's headers: the S3-meaningful ones are signed, the rest
        // only sent.
        let mut unsigned = Vec::new();
        for (k, v) in headers {
            let k = k.to_ascii_lowercase();
            if k.starts_with("x-amz-") || matches!(k.as_str(), "content-type" | "range" | "content-md5" | "cache-control") {
                signed.push((k, v.clone()));
            } else {
                unsigned.push((k, v.clone()));
            }
        }
        let auth = sigv4::authorization(&self.cfg.credentials, method.as_str(), &path, query, &signed, &payload, &date);
        let mut qs: Vec<(String, String)> = query.iter().map(|(k, v)| (sigv4::uri_encode(k, false), sigv4::uri_encode(v, false))).collect();
        qs.sort();
        let q = qs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let url = format!("{}://{host}{path}{}", self.scheme, if q.is_empty() { String::new() } else { format!("?{q}") });
        let mut req = Request::new(method, Url::parse(&url).map_err(|e| S3Error::Config(e.to_string()))?).header("Host", host).header("Authorization", auth);
        // Every signed header goes out exactly as signed (content-type with the body).
        let mut content_type = None;
        for (k, v) in signed.into_iter().chain(unsigned) {
            match k.as_str() {
                "host" => {}
                "content-type" => content_type = Some(v),
                _ => req = req.header(k, v),
            }
        }
        if !body.is_empty() || matches!(method, Method::Put | Method::Post) {
            req = req.body(content_type.as_deref().unwrap_or("application/octet-stream"), body);
        }
        let resp = self.http.send(req).await.map_err(|e| S3Error::Connection(e.to_string()))?;
        if resp.is_success() {
            return Ok(resp);
        }
        Err(error_of(&resp))
    }

    /// Create a bucket (`Ok(false)`: it already existed and is yours).
    pub async fn create_bucket(&self, bucket: &str) -> S3Result<bool> {
        let body = if self.cfg.credentials.region == "us-east-1" {
            Vec::new()
        } else {
            format!("<CreateBucketConfiguration><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>", xml::escape(&self.cfg.credentials.region)).into_bytes()
        };
        let h = if body.is_empty() { vec![] } else { vec![("content-type".to_string(), "application/xml".to_string())] };
        match self.call(Method::Put, bucket, "", &[], &h, body).await {
            Ok(_) => Ok(true),
            Err(S3Error::Service { code, .. }) if code == "BucketAlreadyOwnedByYou" => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Whether a bucket exists (and is reachable with these credentials).
    pub async fn bucket_exists(&self, bucket: &str) -> S3Result<bool> {
        match self.call(Method::Head, bucket, "", &[], &[], Vec::new()).await {
            Ok(_) => Ok(true),
            Err(S3Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Delete an (empty) bucket.
    pub async fn delete_bucket(&self, bucket: &str) -> S3Result<()> {
        self.call(Method::Delete, bucket, "", &[], &[], Vec::new()).await.map(|_| ())
    }

    /// Store an object; returns its ETag.
    pub async fn put(&self, bucket: &str, key: &str, body: impl Into<Vec<u8>>, opts: &PutOptions) -> S3Result<String> {
        let mut h = Vec::new();
        if let Some(ct) = &opts.content_type {
            h.push(("content-type".to_string(), ct.clone()));
        }
        if let Some(cc) = &opts.cache_control {
            h.push(("cache-control".to_string(), cc.clone()));
        }
        for (k, v) in &opts.metadata {
            h.push((format!("x-amz-meta-{}", k.to_ascii_lowercase()), v.clone()));
        }
        let r = self.call(Method::Put, bucket, key, &[], &h, body.into()).await?;
        Ok(unquote(r.header("etag").unwrap_or_default()))
    }

    fn object_of(r: Response) -> Object {
        let size = r
            .header("content-range")
            .and_then(|v| v.rsplit('/').next())
            .and_then(|t| t.parse().ok())
            .or_else(|| r.header("content-length").and_then(|v| v.parse().ok()))
            .unwrap_or(r.body.len() as u64);
        let metadata = r.headers().filter_map(|(k, v)| k.to_ascii_lowercase().strip_prefix("x-amz-meta-").map(|n| (n.to_string(), v.to_string()))).collect();
        Object {
            content_type: r.header("content-type").map(String::from),
            etag: unquote(r.header("etag").unwrap_or_default()),
            size,
            metadata,
            body: r.body,
        }
    }

    /// Read an object (`None` when there is no such key).
    pub async fn get(&self, bucket: &str, key: &str) -> S3Result<Option<Object>> {
        match self.call(Method::Get, bucket, key, &[], &[], Vec::new()).await {
            Ok(r) => Ok(Some(Self::object_of(r))),
            Err(S3Error::NotFound { code, .. }) if code == "NoSuchKey" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Read bytes `start..=end` of an object.
    pub async fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> S3Result<Object> {
        let r = self.call(Method::Get, bucket, key, &[], &[("range".into(), format!("bytes={start}-{end}"))], Vec::new()).await?;
        Ok(Self::object_of(r))
    }

    /// An object's size, ETag and metadata without its body (`None`: no such key).
    pub async fn head(&self, bucket: &str, key: &str) -> S3Result<Option<Object>> {
        match self.call(Method::Head, bucket, key, &[], &[], Vec::new()).await {
            Ok(r) => Ok(Some(Object { body: Vec::new(), ..Self::object_of(r) })),
            Err(S3Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete an object (absent is fine — S3 says so too).
    pub async fn delete(&self, bucket: &str, key: &str) -> S3Result<()> {
        self.call(Method::Delete, bucket, key, &[], &[], Vec::new()).await.map(|_| ())
    }

    /// Copy an object (server side); returns the new ETag.
    pub async fn copy(&self, src_bucket: &str, src_key: &str, bucket: &str, key: &str) -> S3Result<String> {
        let src = format!("/{src_bucket}/{}", sigv4::uri_encode(src_key, true));
        let r = self.call(Method::Put, bucket, key, &[], &[("x-amz-copy-source".into(), src)], Vec::new()).await?;
        let text = r.text();
        // A copy can fail with 200 and an error document.
        if xml::first(&text, "Error").is_some() {
            return Err(error_doc(200, &text));
        }
        Ok(unquote(&xml::text(&text, "ETag").unwrap_or_default()))
    }

    /// One page of keys under `prefix` (`delimiter` groups "directories").
    pub async fn list(&self, bucket: &str, prefix: &str, delimiter: Option<&str>, max_keys: u32, token: Option<&str>) -> S3Result<ListPage> {
        let mut q = vec![("list-type".to_string(), "2".to_string()), ("prefix".to_string(), prefix.to_string()), ("max-keys".to_string(), max_keys.to_string())];
        if let Some(d) = delimiter {
            q.push(("delimiter".into(), d.into()));
        }
        if let Some(t) = token {
            q.push(("continuation-token".into(), t.into()));
        }
        let r = self.call(Method::Get, bucket, "", &q, &[], Vec::new()).await?;
        let text = r.text();
        let objects = xml::all(&text, "Contents")
            .into_iter()
            .map(|c| ObjectInfo {
                key: xml::text(c, "Key").unwrap_or_default(),
                size: xml::text(c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                etag: unquote(&xml::text(c, "ETag").unwrap_or_default()),
                last_modified: xml::text(c, "LastModified").unwrap_or_default(),
            })
            .collect();
        let common_prefixes = xml::all(&text, "CommonPrefixes").into_iter().filter_map(|c| xml::text(c, "Prefix")).collect();
        let truncated = xml::text(&text, "IsTruncated").as_deref() == Some("true");
        Ok(ListPage { objects, common_prefixes, next: if truncated { xml::text(&text, "NextContinuationToken") } else { None } })
    }

    /// Every key under `prefix`, following the pages.
    pub async fn list_all(&self, bucket: &str, prefix: &str) -> S3Result<Vec<ObjectInfo>> {
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let page = self.list(bucket, prefix, None, 1000, token.as_deref()).await?;
            out.extend(page.objects);
            match page.next {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }

    /// Delete every object under `prefix`; returns how many.
    pub async fn delete_prefix(&self, bucket: &str, prefix: &str) -> S3Result<usize> {
        let all = self.list_all(bucket, prefix).await?;
        for o in &all {
            self.delete(bucket, &o.key).await?;
        }
        Ok(all.len())
    }

    /// Start a multipart upload; returns its upload id.
    pub async fn multipart_start(&self, bucket: &str, key: &str, opts: &PutOptions) -> S3Result<String> {
        let mut h = Vec::new();
        if let Some(ct) = &opts.content_type {
            h.push(("content-type".to_string(), ct.clone()));
        }
        for (k, v) in &opts.metadata {
            h.push((format!("x-amz-meta-{}", k.to_ascii_lowercase()), v.clone()));
        }
        let r = self.call(Method::Post, bucket, key, &[("uploads".into(), String::new())], &h, Vec::new()).await?;
        xml::text(&r.text(), "UploadId").ok_or_else(|| S3Error::Protocol("no UploadId in the answer".into()))
    }

    /// Upload part `n` (1-based; every part but the last ≥ 5 MiB); returns its ETag.
    pub async fn multipart_part(&self, bucket: &str, key: &str, upload_id: &str, n: u32, data: Vec<u8>) -> S3Result<String> {
        let q = [("partNumber".to_string(), n.to_string()), ("uploadId".to_string(), upload_id.to_string())];
        let r = self.call(Method::Put, bucket, key, &q, &[], data).await?;
        Ok(unquote(r.header("etag").unwrap_or_default()))
    }

    /// Complete an upload from its parts' ETags (in order); returns the object's ETag.
    pub async fn multipart_complete(&self, bucket: &str, key: &str, upload_id: &str, etags: &[String]) -> S3Result<String> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (i, e) in etags.iter().enumerate() {
            body.push_str(&format!("<Part><PartNumber>{}</PartNumber><ETag>\"{}\"</ETag></Part>", i + 1, xml::escape(e)));
        }
        body.push_str("</CompleteMultipartUpload>");
        let r = self.call(Method::Post, bucket, key, &[("uploadId".into(), upload_id.into())], &[("content-type".into(), "application/xml".into())], body.into_bytes()).await?;
        let text = r.text();
        if xml::first(&text, "Error").is_some() {
            return Err(error_doc(200, &text));
        }
        Ok(unquote(&xml::text(&text, "ETag").unwrap_or_default()))
    }

    /// Abandon an upload (its parts are discarded).
    pub async fn multipart_abort(&self, bucket: &str, key: &str, upload_id: &str) -> S3Result<()> {
        self.call(Method::Delete, bucket, key, &[("uploadId".into(), upload_id.into())], &[], Vec::new()).await.map(|_| ())
    }

    /// Store a large object in parts of `part_size` (≥ 5 MiB); aborts the
    /// upload when a part fails. Returns the object's ETag.
    pub async fn put_multipart(&self, bucket: &str, key: &str, body: &[u8], part_size: usize, opts: &PutOptions) -> S3Result<String> {
        let part_size = part_size.max(5 << 20);
        let id = self.multipart_start(bucket, key, opts).await?;
        let mut etags = Vec::new();
        for (i, chunk) in body.chunks(part_size).enumerate() {
            match self.multipart_part(bucket, key, &id, i as u32 + 1, chunk.to_vec()).await {
                Ok(e) => etags.push(e),
                Err(e) => {
                    let _ = self.multipart_abort(bucket, key, &id).await;
                    return Err(e);
                }
            }
        }
        if etags.is_empty() {
            etags.push(self.multipart_part(bucket, key, &id, 1, Vec::new()).await?);
        }
        self.multipart_complete(bucket, key, &id, &etags).await
    }

    /// A URL anyone can GET the object with, for `expires` (≤ 7 days).
    pub fn presign_get(&self, bucket: &str, key: &str, expires: Duration) -> S3Result<String> {
        self.presign("GET", bucket, key, expires)
    }

    /// A URL anyone can PUT the object with (a browser uploading directly).
    pub fn presign_put(&self, bucket: &str, key: &str, expires: Duration) -> S3Result<String> {
        self.presign("PUT", bucket, key, expires)
    }

    fn presign(&self, method: &str, bucket: &str, key: &str, expires: Duration) -> S3Result<String> {
        check_bucket(bucket)?;
        if expires.as_secs() == 0 || expires.as_secs() > 604_800 {
            return Err(S3Error::Config("presigned URLs last 1 s to 7 days".into()));
        }
        let (host, path) = self.locate(bucket, key);
        let q = sigv4::presign_query(&self.cfg.credentials, method, &host, &path, &[], &sigv4::amz_date(now_unix()), expires.as_secs());
        Ok(format!("{}://{host}{path}?{q}", self.scheme))
    }
}

fn error_doc(status: u16, text: &str) -> S3Error {
    let code = xml::text(text, "Code").unwrap_or_else(|| format!("HTTP{status}"));
    let message = xml::text(text, "Message").unwrap_or_default();
    match status {
        403 => S3Error::Denied { code, message },
        404 => S3Error::NotFound { code, message },
        _ => S3Error::Service { status, code, message },
    }
}

fn error_of(r: &Response) -> S3Error {
    let text = r.text();
    // HEAD answers carry no body: name the code from the status.
    if text.trim().is_empty() {
        let code = match r.status {
            404 => "NotFound",
            403 => "Forbidden",
            _ => "Error",
        };
        return error_doc(r.status, &format!("<Error><Code>{code}</Code></Error>"));
    }
    error_doc(r.status, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressing_and_names() {
        let c = S3Client::new(S3Config::new("http://127.0.0.1:9000/", "k", "s", "us-east-1")).unwrap();
        assert_eq!(c.locate("pos-media", "ảnh/khách 1.jpg"), ("127.0.0.1:9000".to_string(), "/pos-media/%E1%BA%A3nh/kh%C3%A1ch%201.jpg".to_string()));
        assert_eq!(c.locate("pos-media", ""), ("127.0.0.1:9000".to_string(), "/pos-media".to_string()));
        let mut cfg = S3Config::new("https://s3.us-east-1.amazonaws.com", "k", "s", "us-east-1");
        cfg.path_style = false;
        let v = S3Client::new(cfg).unwrap();
        assert_eq!(v.locate("pos-media", "a.jpg"), ("pos-media.s3.us-east-1.amazonaws.com".to_string(), "/a.jpg".to_string()));
        for bad in ["ab", "Upper", "-lead", "trail-", "under_score", &"x".repeat(64)] {
            assert!(check_bucket(bad).is_err(), "{bad}");
        }
        assert!(check_bucket("pos-media.backup-2026").is_ok());
        assert!(c.presign_get("pos-media", "a", Duration::from_secs(8 * 86_400)).is_err());
        let url = c.presign_get("pos-media", "a b.jpg", Duration::from_secs(600)).unwrap();
        assert!(url.starts_with("http://127.0.0.1:9000/pos-media/a%20b.jpg?X-Amz-Algorithm=AWS4-HMAC-SHA256&"), "{url}");
        assert!(url.contains("X-Amz-Expires=600") && url.contains("&X-Amz-Signature="));
    }

    #[test]
    fn error_documents() {
        let e = error_doc(404, "<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>");
        assert_eq!(e, S3Error::NotFound { code: "NoSuchKey".into(), message: "The specified key does not exist.".into() });
        assert!(matches!(error_doc(403, "<Error><Code>SignatureDoesNotMatch</Code></Error>"), S3Error::Denied { .. }));
        assert!(matches!(error_doc(409, "<Error><Code>BucketNotEmpty</Code></Error>"), S3Error::Service { status: 409, .. }));
    }
}
