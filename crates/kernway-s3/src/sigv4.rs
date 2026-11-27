//! AWS Signature Version 4 for S3: signed request headers and presigned
//! URLs. HMAC-SHA256 and SHA-256 come from `kernway-wire` (shared with the
//! SCRAM clients); the canonical forms are written out here.

use kernway_wire::scram::Hash;

/// The SHA-256 of an empty payload.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Credentials and region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// Access key id.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: String,
    /// Session token (temporary credentials), if any.
    pub session_token: Option<String>,
    /// Region (`us-east-1` for MinIO and most S3-compatible stores).
    pub region: String,
}

/// Lowercase hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// SHA-256 as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Hash::Sha256.hash(data))
}

/// URI-encode per SigV4: unreserved characters kept, everything else
/// `%XX`; `/` kept when `keep_slash` (object key paths).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `YYYYMMDD'T'HHMMSS'Z'` for a Unix time (UTC).
pub fn amz_date(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

fn signing_key(c: &Credentials, date: &str) -> Vec<u8> {
    let k = Hash::Sha256.hmac(format!("AWS4{}", c.secret_key).as_bytes(), date.as_bytes());
    let k = Hash::Sha256.hmac(&k, c.region.as_bytes());
    let k = Hash::Sha256.hmac(&k, b"s3");
    Hash::Sha256.hmac(&k, b"aws4_request")
}

/// The canonical query string: parameters sorted, keys and values encoded.
fn canonical_query(query: &[(String, String)]) -> String {
    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Sign a request: `headers` must hold every header to sign (host,
/// x-amz-date, x-amz-content-sha256 …), names in any case. Returns the
/// `Authorization` header value.
pub fn authorization(c: &Credentials, method: &str, path: &str, query: &[(String, String)], headers: &[(String, String)], payload_hash: &str, amz_date: &str) -> String {
    let mut h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: String = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{method}\n{path}\n{}\n{canonical_headers}\n{signed}\n{payload_hash}", canonical_query(query));
    let date = &amz_date[..8];
    let scope = format!("{date}/{}/s3/aws4_request", c.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let sig = hex(&Hash::Sha256.hmac(&signing_key(c, date), to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", c.access_key)
}

/// A presigned URL's query string (host the only signed header, the payload
/// unsigned), valid for `expires_secs`.
pub fn presign_query(c: &Credentials, method: &str, host: &str, path: &str, extra: &[(String, String)], amz_date: &str, expires_secs: u64) -> String {
    let date = &amz_date[..8];
    let scope = format!("{date}/{}/s3/aws4_request", c.region);
    let mut q: Vec<(String, String)> = extra.to_vec();
    q.push(("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()));
    q.push(("X-Amz-Credential".into(), format!("{}/{scope}", c.access_key)));
    q.push(("X-Amz-Date".into(), amz_date.into()));
    q.push(("X-Amz-Expires".into(), expires_secs.to_string()));
    if let Some(t) = &c.session_token {
        q.push(("X-Amz-Security-Token".into(), t.clone()));
    }
    q.push(("X-Amz-SignedHeaders".into(), "host".into()));
    let canonical = format!("{method}\n{path}\n{}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD", canonical_query(&q));
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let sig = hex(&Hash::Sha256.hmac(&signing_key(c, date), to_sign.as_bytes()));
    format!("{}&X-Amz-Signature={sig}", canonical_query(&q))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Credentials {
        // The credentials of the AWS S3 SigV4 documentation examples.
        Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            region: "us-east-1".into(),
        }
    }

    #[test]
    fn matches_the_aws_get_object_example() {
        // "Example: GET Object" (Authenticating Requests: Using the Authorization Header).
        let headers = vec![
            ("Host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("Range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = authorization(&example(), "GET", "/test.txt", &[], &headers, EMPTY_SHA256, "20130524T000000Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn matches_the_aws_presigned_url_example() {
        // "Example: Presigned URL" (Authenticating Requests: Using Query Parameters).
        let q = presign_query(&example(), "GET", "examplebucket.s3.amazonaws.com", "/test.txt", &[], "20130524T000000Z", 86_400);
        assert!(q.ends_with("X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"), "{q}");
        assert!(q.contains("X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request"));
    }

    #[test]
    fn dates_and_encoding() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(amz_date(951_782_400 + 3661), "20000229T010101Z", "leap day");
        assert_eq!(uri_encode("ảnh/khách hàng 1.jpg", true), "%E1%BA%A3nh/kh%C3%A1ch%20h%C3%A0ng%201.jpg");
        assert_eq!(uri_encode("a/b+c=d", false), "a%2Fb%2Bc%3Dd");
        assert_eq!(sha256_hex(b""), EMPTY_SHA256);
    }
}
