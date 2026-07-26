//! JSON Web Tokens (RFC 7519), HS256 — a standards-compliant, stateless bearer token
//! (feature = `jwt`).
//!
//! Distinct from the session [`token`](crate::token): that is a *stateful* ticket into
//! the session registry (so it can be revoked); a JWT is *stateless* — everything a
//! verifier needs is inside it, verified by a shared secret, with nothing to look up.
//! Use it for APIs, service-to-service calls, or interop with an issuer like Keycloak
//! or Auth0. It cannot be revoked before `exp`, so keep lifetimes short.
//!
//! A token is `base64url(header) . base64url(payload) . base64url(signature)`:
//! - header — `{"alg":"HS256","typ":"JWT"}`
//! - payload — the [`Claims`] JSON (RFC 7519 §4 registered claims + your own)
//! - signature — `HMAC-SHA256(secret, header_b64 "." payload_b64)`
//!
//! ## Validation is the whole point
//!
//! [`Jwt::decode`] **rejects** anything it cannot trust: a bad signature
//! ([`JwtError::InvalidSignature`]), a token whose header names any algorithm other
//! than HS256 ([`JwtError::UnsupportedAlg`] — this is what defeats the `alg:none` and
//! RS256-as-HS256 confusion attacks), or an expired / not-yet-valid token
//! ([`JwtError::Expired`] / [`JwtError::NotYetValid`]). The signature is checked in
//! constant time, *before* any claim is trusted.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::hmac_sha256;
use crate::token::{b64url_decode, b64url_encode};

/// The fixed header for the only algorithm we issue.
const HEADER_JSON: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// A JWT could not be produced, or (far more often) an incoming one could not be
/// trusted. Everything but [`Serialization`](JwtError::Serialization) is a *rejection*
/// of an untrusted token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    /// Not `header.payload.signature`, or a part is not valid base64url / JSON.
    Malformed,
    /// The header's `alg` is not the expected one (includes `none`) — refused, not verified.
    UnsupportedAlg,
    /// No key in the JWKS matches the token's `kid` (RS256) — cannot verify.
    UnknownKey,
    /// The signature does not match — the token was forged or tampered with.
    InvalidSignature,
    /// `exp` is in the past (beyond the leeway).
    Expired,
    /// `nbf` (or `iat`) is in the future (beyond the leeway).
    NotYetValid,
    /// `iss` did not match the expected issuer.
    InvalidIssuer,
    /// `aud` did not match the expected audience.
    InvalidAudience,
    /// The claims could not be serialised when encoding.
    Serialization,
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            JwtError::Malformed => "malformed token",
            JwtError::UnsupportedAlg => "unsupported algorithm",
            JwtError::UnknownKey => "no matching key in the JWKS",
            JwtError::InvalidSignature => "invalid signature",
            JwtError::Expired => "token expired",
            JwtError::NotYetValid => "token not yet valid",
            JwtError::InvalidIssuer => "unexpected issuer",
            JwtError::InvalidAudience => "unexpected audience",
            JwtError::Serialization => "could not serialise claims",
        };
        f.write_str(message)
    }
}

impl std::error::Error for JwtError {}

/// The token's claims — the RFC 7519 §4 registered claims (all optional) plus any
/// custom claims you add. Serialises to the JWT payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Claims {
    /// `sub` — the subject (usually the user id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    /// `iss` — the issuer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// `aud` — the audience.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// `exp` — expiry, unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,
    /// `nbf` — not valid before, unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbf: Option<u64>,
    /// `iat` — issued at, unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<u64>,
    /// Any additional (custom) claims, e.g. `roles`.
    #[serde(flatten)]
    pub custom: BTreeMap<String, Value>,
}

impl Claims {
    /// Empty claims to build on.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `sub`.
    #[must_use]
    pub fn subject(mut self, sub: impl Into<String>) -> Self {
        self.sub = Some(sub.into());
        self
    }

    /// Set `iss`.
    #[must_use]
    pub fn issuer(mut self, iss: impl Into<String>) -> Self {
        self.iss = Some(iss.into());
        self
    }

    /// Set `aud`.
    #[must_use]
    pub fn audience(mut self, aud: impl Into<String>) -> Self {
        self.aud = Some(aud.into());
        self
    }

    /// Set `exp` (unix seconds).
    #[must_use]
    pub fn expires_at(mut self, exp: u64) -> Self {
        self.exp = Some(exp);
        self
    }

    /// Set `nbf` (unix seconds).
    #[must_use]
    pub fn not_before(mut self, nbf: u64) -> Self {
        self.nbf = Some(nbf);
        self
    }

    /// Set `iat` (unix seconds).
    #[must_use]
    pub fn issued_at(mut self, iat: u64) -> Self {
        self.iat = Some(iat);
        self
    }

    /// Add a custom claim.
    #[must_use]
    pub fn claim(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.custom.insert(key.into(), value.into());
        self
    }

    /// Set a `roles` array claim (the common shape a `BearerAuth` reads into a
    /// `SecurityContext`).
    #[must_use]
    pub fn roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let array = roles.into_iter().map(|r| Value::String(r.into())).collect();
        self.custom.insert("roles".into(), Value::Array(array));
        self
    }

    /// Read the `roles` array claim (empty if absent or not an array of strings).
    #[must_use]
    pub fn role_list(&self) -> Vec<String> {
        self.custom
            .get("roles")
            .and_then(Value::as_array)
            .map(|array| array.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }
}

/// What to check when decoding, beyond the signature. Signature and algorithm are
/// *always* checked; these are the time/identity claims.
#[derive(Debug, Clone)]
pub struct Validation {
    /// Clock-skew tolerance in seconds for `exp`/`nbf` (default 60).
    pub leeway_secs: u64,
    /// Reject if `exp` is present and passed (default `true`).
    pub validate_exp: bool,
    /// Reject if `nbf` is present and in the future (default `true`).
    pub validate_nbf: bool,
    /// If set, `iss` must equal this.
    pub expected_iss: Option<String>,
    /// If set, `aud` must equal this.
    pub expected_aud: Option<String>,
}

impl Default for Validation {
    fn default() -> Self {
        Self {
            leeway_secs: 60,
            validate_exp: true,
            validate_nbf: true,
            expected_iss: None,
            expected_aud: None,
        }
    }
}

/// Signs and verifies HS256 JWTs with a shared secret.
pub struct Jwt {
    key: Vec<u8>,
}

impl Jwt {
    /// A codec over an HMAC secret. Keep it secret; anyone with it can mint tokens.
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self { key: key.into() }
    }

    /// Sign `claims` into a compact JWT string.
    pub fn encode(&self, claims: &Claims) -> Result<String, JwtError> {
        let payload = serde_json::to_vec(claims).map_err(|_| JwtError::Serialization)?;
        let signing_input = format!("{}.{}", b64url_encode(HEADER_JSON.as_bytes()), b64url_encode(&payload));
        let signature = b64url_encode(&hmac_sha256(&self.key, signing_input.as_bytes()));
        Ok(format!("{signing_input}.{signature}"))
    }

    /// Verify and decode a token with the default [`Validation`], at time `now` (unix
    /// seconds — passed in so the caller owns the clock, as the session store does).
    pub fn decode(&self, token: &str, now: u64) -> Result<Claims, JwtError> {
        self.decode_with(token, now, &Validation::default())
    }

    /// Verify and decode a token against an explicit [`Validation`].
    pub fn decode_with(&self, token: &str, now: u64, validation: &Validation) -> Result<Claims, JwtError> {
        // Split into exactly three parts.
        let mut parts = token.split('.');
        let (header_b64, payload_b64, signature_b64) =
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(h), Some(p), Some(s), None) => (h, p, s),
                _ => return Err(JwtError::Malformed),
            };

        // Verify the signature FIRST, in constant time, before trusting any bytes.
        let signing_input = format!("{header_b64}.{payload_b64}");
        let expected = b64url_encode(&hmac_sha256(&self.key, signing_input.as_bytes()));
        if !crate::csrf::verify(signature_b64, &expected) {
            return Err(JwtError::InvalidSignature);
        }

        // The header must declare HS256 — refuse `none` and any asymmetric alg we
        // cannot verify with a MAC (defeats alg-confusion).
        let header_bytes = b64url_decode(header_b64).ok_or(JwtError::Malformed)?;
        let header: Header = serde_json::from_slice(&header_bytes).map_err(|_| JwtError::Malformed)?;
        if header.alg != "HS256" {
            return Err(JwtError::UnsupportedAlg);
        }

        // Only now decode and validate the claims.
        let payload_bytes = b64url_decode(payload_b64).ok_or(JwtError::Malformed)?;
        let claims: Claims = serde_json::from_slice(&payload_bytes).map_err(|_| JwtError::Malformed)?;
        validate_claims(&claims, now, validation)?;
        Ok(claims)
    }
}

/// Check the time + identity claims against `validation` — shared by HS256
/// ([`Jwt::decode`]) and RS256 ([`Jwks::verify`]).
pub(crate) fn validate_claims(claims: &Claims, now: u64, validation: &Validation) -> Result<(), JwtError> {
    if validation.validate_exp {
        if let Some(exp) = claims.exp {
            if now > exp.saturating_add(validation.leeway_secs) {
                return Err(JwtError::Expired);
            }
        }
    }
    if validation.validate_nbf {
        if let Some(nbf) = claims.nbf {
            if now.saturating_add(validation.leeway_secs) < nbf {
                return Err(JwtError::NotYetValid);
            }
        }
    }
    if let Some(expected_iss) = &validation.expected_iss {
        if claims.iss.as_deref() != Some(expected_iss.as_str()) {
            return Err(JwtError::InvalidIssuer);
        }
    }
    if let Some(expected_aud) = &validation.expected_aud {
        if claims.aud.as_deref() != Some(expected_aud.as_str()) {
            return Err(JwtError::InvalidAudience);
        }
    }
    Ok(())
}

/// The JWT header, parsed only to read `alg`.
#[derive(Deserialize)]
struct Header {
    alg: String,
}

/// RS256 verification against a JSON Web Key Set (feature = `jwks`).
#[cfg(feature = "jwks")]
mod jwks {
    use ring::signature::{RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256};
    use serde::Deserialize;

    use super::{validate_claims, Claims, JwtError, Validation};
    use crate::token::b64url_decode;

    /// A set of public keys from an issuer's JWKS document, for verifying RS256 tokens
    /// they signed. Fetch the JSON from the issuer's `jwks_uri` (their public keys) and
    /// parse it with [`from_json`](Jwks::from_json); a `JwksClient` (kernway-oauth2)
    /// does the fetching + caching.
    pub struct Jwks {
        keys: Vec<RsaKey>,
    }

    /// One RSA public key: its `kid` and the modulus/exponent bytes.
    struct RsaKey {
        kid: Option<String>,
        n: Vec<u8>,
        e: Vec<u8>,
    }

    #[derive(Deserialize)]
    struct RawJwks {
        keys: Vec<RawJwk>,
    }

    #[derive(Deserialize)]
    struct RawJwk {
        kty: String,
        #[serde(default)]
        kid: Option<String>,
        n: Option<String>,
        e: Option<String>,
    }

    #[derive(Deserialize)]
    struct RsaHeader {
        alg: String,
        #[serde(default)]
        kid: Option<String>,
    }

    impl Jwks {
        /// Parse a JWKS JSON document, keeping the RSA signing keys.
        pub fn from_json(bytes: &[u8]) -> Result<Jwks, JwtError> {
            let raw: RawJwks = serde_json::from_slice(bytes).map_err(|_| JwtError::Malformed)?;
            let keys = raw
                .keys
                .into_iter()
                .filter(|k| k.kty == "RSA")
                .filter_map(|k| {
                    Some(RsaKey { kid: k.kid, n: b64url_decode(k.n.as_deref()?)?, e: b64url_decode(k.e.as_deref()?)? })
                })
                .collect();
            Ok(Jwks { keys })
        }

        /// Whether this set contains a key with the given `kid`.
        #[must_use]
        pub fn contains_kid(&self, kid: &str) -> bool {
            self.keys.iter().any(|k| k.kid.as_deref() == Some(kid))
        }

        fn find(&self, kid: Option<&str>) -> Option<&RsaKey> {
            match kid {
                Some(kid) => self.keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
                None => self.keys.first(), // no kid → the only/first key
            }
        }

        /// Verify an **RS256** `token` against these keys and validate its claims. The
        /// signature is checked with the RSA public key whose `kid` matches the token
        /// header, before any claim is trusted. `now` is unix seconds.
        pub fn verify(&self, token: &str, now: u64, validation: &Validation) -> Result<Claims, JwtError> {
            let mut parts = token.split('.');
            let (header_b64, payload_b64, signature_b64) =
                match (parts.next(), parts.next(), parts.next(), parts.next()) {
                    (Some(h), Some(p), Some(s), None) => (h, p, s),
                    _ => return Err(JwtError::Malformed),
                };

            // Header → algorithm + key id. Only RS256 here.
            let header_bytes = b64url_decode(header_b64).ok_or(JwtError::Malformed)?;
            let header: RsaHeader = serde_json::from_slice(&header_bytes).map_err(|_| JwtError::Malformed)?;
            if header.alg != "RS256" {
                return Err(JwtError::UnsupportedAlg);
            }
            let key = self.find(header.kid.as_deref()).ok_or(JwtError::UnknownKey)?;

            // Verify the RSA signature over `header.payload` BEFORE trusting the claims.
            let signing_input = format!("{header_b64}.{payload_b64}");
            let signature = b64url_decode(signature_b64).ok_or(JwtError::Malformed)?;
            let public_key = RsaPublicKeyComponents { n: &key.n, e: &key.e };
            public_key
                .verify(&RSA_PKCS1_2048_8192_SHA256, signing_input.as_bytes(), &signature)
                .map_err(|_| JwtError::InvalidSignature)?;

            let claims: Claims = serde_json::from_slice(&b64url_decode(payload_b64).ok_or(JwtError::Malformed)?)
                .map_err(|_| JwtError::Malformed)?;
            validate_claims(&claims, now, validation)?;
            Ok(claims)
        }
    }
}

#[cfg(feature = "jwks")]
pub use jwks::Jwks;

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn jwt() -> Jwt {
        Jwt::new("shared-secret")
    }

    #[test]
    fn a_signed_token_round_trips_its_claims() {
        let claims = Claims::new()
            .subject("alice")
            .issuer("kernway")
            .expires_at(NOW + 3600)
            .roles(["ADMIN", "USER"]);
        let token = jwt().encode(&claims).unwrap();
        // Exactly three base64url parts.
        assert_eq!(token.split('.').count(), 3);
        let decoded = jwt().decode(&token, NOW).unwrap();
        assert_eq!(decoded.sub.as_deref(), Some("alice"));
        assert_eq!(decoded.role_list(), vec!["ADMIN".to_string(), "USER".to_string()]);
    }

    #[test]
    fn the_header_is_standard_hs256() {
        let token = jwt().encode(&Claims::new().subject("x")).unwrap();
        let header_b64 = token.split('.').next().unwrap();
        let header = String::from_utf8(b64url_decode(header_b64).unwrap()).unwrap();
        assert_eq!(header, r#"{"alg":"HS256","typ":"JWT"}"#);
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let token = jwt().encode(&Claims::new().subject("alice").roles(["USER"])).unwrap();
        // Re-encode a payload that grants ADMIN, keep the original signature.
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = b64url_encode(
            &serde_json::to_vec(&Claims::new().subject("alice").roles(["ADMIN"])).unwrap(),
        );
        parts[1] = &forged;
        let forged_token = parts.join(".");
        assert_eq!(jwt().decode(&forged_token, NOW), Err(JwtError::InvalidSignature));
    }

    #[test]
    fn a_wrong_secret_is_rejected() {
        let token = jwt().encode(&Claims::new().subject("alice")).unwrap();
        assert_eq!(Jwt::new("other-secret").decode(&token, NOW), Err(JwtError::InvalidSignature));
    }

    #[test]
    fn the_alg_none_attack_is_refused() {
        // Craft header {"alg":"none"} + the same payload, and (per the attack) an
        // empty signature. Our HMAC check fails first, but even a matching MAC would
        // be refused by the alg check.
        let payload = b64url_encode(&serde_json::to_vec(&Claims::new().subject("attacker").roles(["ADMIN"])).unwrap());
        let header = b64url_encode(br#"{"alg":"none","typ":"JWT"}"#);
        let none_token = format!("{header}.{payload}.");
        let err = jwt().decode(&none_token, NOW).unwrap_err();
        assert!(matches!(err, JwtError::InvalidSignature | JwtError::UnsupportedAlg), "must never accept alg:none, got {err:?}");
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let token = jwt().encode(&Claims::new().subject("alice").expires_at(NOW - 3600)).unwrap();
        assert_eq!(jwt().decode(&token, NOW), Err(JwtError::Expired));
        // Within the leeway it is still fine.
        let border = jwt().encode(&Claims::new().subject("alice").expires_at(NOW - 30)).unwrap();
        assert!(jwt().decode(&border, NOW).is_ok(), "30s past exp is inside the 60s leeway");
    }

    #[test]
    fn a_not_yet_valid_token_is_rejected() {
        let token = jwt().encode(&Claims::new().subject("alice").not_before(NOW + 3600)).unwrap();
        assert_eq!(jwt().decode(&token, NOW), Err(JwtError::NotYetValid));
    }

    #[test]
    fn issuer_and_audience_are_checked_when_expected() {
        let token = jwt().encode(&Claims::new().subject("a").issuer("kernway").audience("web")).unwrap();
        let ok = Validation { expected_iss: Some("kernway".into()), expected_aud: Some("web".into()), ..Default::default() };
        assert!(jwt().decode_with(&token, NOW, &ok).is_ok());
        let wrong_iss = Validation { expected_iss: Some("evil".into()), ..Default::default() };
        assert_eq!(jwt().decode_with(&token, NOW, &wrong_iss), Err(JwtError::InvalidIssuer));
        let wrong_aud = Validation { expected_aud: Some("mobile".into()), ..Default::default() };
        assert_eq!(jwt().decode_with(&token, NOW, &wrong_aud), Err(JwtError::InvalidAudience));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        assert_eq!(jwt().decode("not-a-jwt", NOW), Err(JwtError::Malformed));
        assert_eq!(jwt().decode("only.two", NOW), Err(JwtError::Malformed));
        assert_eq!(jwt().decode("a.b.c.d", NOW), Err(JwtError::Malformed));
    }

    // A real RS256 token + its JWKS (generated with an RSA-2048 key). Proves we verify
    // a token signed by an external issuer using only their published public key.
    #[cfg(feature = "jwks")]
    const RS256_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6InRlc3Qta2V5LTEifQ.eyJzdWIiOiIxMjM0NTY3ODkwIiwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZSIsImF1ZCI6Im15LWFwaSIsImV4cCI6NDEwMjQ0NDgwMCwiaWF0IjoxNzAwMDAwMDAwfQ.j5o1170qMzys95RZy2KKrssW-KuQ-KRbRkYjoVOeIxa9oReChs2sea_qE943SApq58XLvswxZUCnO2vZ7IPz82pGc6gYFG1uXiDnyJOCkeSwtkAHWGJ6EWl79rULFGzi4DMNO-y3FcM0VWekeev4Y6NQB_bNAnLPwmyKY3BiRgK9Oo7FwdO8EQNJqXJ_I851pH88P39S9pdSq4He4Hi91xRsGspcSKPNMwMxAgZIXAlF5zVbOr6dLv-jMypGNsu3ev4jBz0V9A58XPGWafdMguwmnPjZwBeASDywqoCJbzH-pxcwvs7V0RnOQs9hUHArsXamD1CvJmWmvflL1hkpJQ";
    #[cfg(feature = "jwks")]
    const RS256_JWKS: &str = r#"{"keys":[{"kty":"RSA","use":"sig","kid":"test-key-1","alg":"RS256","n":"oYaIi_50A0cWJapMnTI2JVjtCt0jdeamBgsLkhaS_IhTMOBUAjCSNxLb4Xx6dFZv3RGrONWc4vTr8mnEwozfiG3YhC0UONBCboWX607pm1fDu6lxL-XqmIx86KrYrVpK7Cv55esuZopGwxglJlC3UiB8Bs_Zf2SFZp5IkHoYz_-uHMlfQpCWQDM-uI1_tNs41c98eLnMU77NIwVV7VgZNHb6AB24_VhYHWxMx2o0so0q3_qdMkMlWCNmMrR1GuabigfwryJFckUe7tuaKRRYzrLLqeCasbcIquatafi8W-7ZGnvNGVvuf7KsnqNUBEsGdyMhw3c26trdMzHCocm7jQ","e":"AQAB"}]}"#;

    #[cfg(feature = "jwks")]
    #[test]
    fn verifies_an_rs256_token_against_a_jwks() {
        let jwks = Jwks::from_json(RS256_JWKS.as_bytes()).unwrap();
        assert!(jwks.contains_kid("test-key-1"));

        let v = Validation {
            expected_iss: Some("https://issuer.example".into()),
            expected_aud: Some("my-api".into()),
            ..Default::default()
        };
        let claims = jwks.verify(RS256_TOKEN, NOW, &v).expect("valid RS256 token verifies");
        assert_eq!(claims.sub.as_deref(), Some("1234567890"));
        assert_eq!(claims.iss.as_deref(), Some("https://issuer.example"));
    }

    #[cfg(feature = "jwks")]
    #[test]
    fn rs256_rejects_tampering_wrong_issuer_and_unknown_key() {
        let jwks = Jwks::from_json(RS256_JWKS.as_bytes()).unwrap();

        // A tampered payload fails the signature.
        let mut parts: Vec<&str> = RS256_TOKEN.split('.').collect();
        let forged = b64url_encode(br#"{"sub":"attacker","iss":"https://issuer.example","aud":"my-api","exp":4102444800}"#);
        parts[1] = &forged;
        let tampered = parts.join(".");
        assert_eq!(jwks.verify(&tampered, NOW, &Validation::default()), Err(JwtError::InvalidSignature));

        // The wrong expected issuer is rejected (after a valid signature).
        let wrong_iss = Validation { expected_iss: Some("https://evil.example".into()), ..Default::default() };
        assert_eq!(jwks.verify(RS256_TOKEN, NOW, &wrong_iss), Err(JwtError::InvalidIssuer));

        // A JWKS without the token's kid cannot verify it.
        let other = Jwks::from_json(br#"{"keys":[{"kty":"RSA","kid":"other","n":"AQAB","e":"AQAB"}]}"#).unwrap();
        assert_eq!(other.verify(RS256_TOKEN, NOW, &Validation::default()), Err(JwtError::UnknownKey));
    }

    #[test]
    fn the_encoding_is_the_standard_signing_input() {
        // header_b64 "." payload_b64 with base64url(JSON) parts, HMAC over exactly
        // that — the RFC 7515 signing input, so a compliant library verifies it.
        let claims = Claims::new().subject("1234567890").issued_at(1_516_239_022);
        let token = jwt().encode(&claims).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        let payload = String::from_utf8(b64url_decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload, r#"{"sub":"1234567890","iat":1516239022}"#, "compact JSON claims");
        let expected_sig = b64url_encode(&hmac_sha256(b"shared-secret", format!("{}.{}", parts[0], parts[1]).as_bytes()));
        assert_eq!(parts[2], expected_sig, "signature is HMAC over header.payload");
    }
}
