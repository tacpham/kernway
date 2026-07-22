//! Multipart/form-data parser — RFC 7578.
//!
//! Parses `multipart/form-data` request bodies into typed fields.
//!
//! # Example
//! ```rust,ignore
//! let form = MultipartForm::from_request(req)?;
//! let name  = form.field("name").and_then(|f| f.as_text()).unwrap_or_default();
//! let file  = form.file("avatar").unwrap();
//! println!("file: {}, size: {}", file.filename.unwrap_or_default(), file.data.len());
//! ```

use kernway_core::request::Request;

/// A single field from a multipart form.
#[derive(Debug, Clone)]
pub struct MultipartField {
    /// Field name (from `Content-Disposition: form-data; name="..."`)
    pub name:         String,
    /// Original filename, if this is a file upload.
    pub filename:     Option<String>,
    /// Content-Type of this field (default: `text/plain`).
    pub content_type: String,
    /// Raw bytes of the field value.
    pub data:         Vec<u8>,
}

impl MultipartField {
    /// Interpret the field data as a UTF-8 string.
    pub fn as_text(&self) -> Option<String> {
        String::from_utf8(self.data.clone()).ok()
    }

    /// Whether this field is a file upload (has a filename).
    pub fn is_file(&self) -> bool { self.filename.is_some() }
}

/// Parsed multipart/form-data form.
#[derive(Debug, Default)]
pub struct MultipartForm {
    fields: Vec<MultipartField>,
}

impl MultipartForm {
    /// Parse from raw bytes and boundary string.
    pub fn parse(body: &[u8], boundary: &str) -> Result<Self, String> {
        let delimiter = format!("--{}", boundary);
        let body_str  = String::from_utf8_lossy(body);

        let mut fields = Vec::new();

        for part in body_str.split(&delimiter) {
            let part = part.trim();
            if part.is_empty() || part == "--" { continue; }

            let split_pos = part.find("\r\n\r\n")
                .map(|p| (p, 4))
                .or_else(|| part.find("\n\n").map(|p| (p, 2)));

            let (header_pos, sep_len) = match split_pos {
                Some(x) => x,
                None    => continue,
            };

            let headers_raw = &part[..header_pos];
            let content     = &part[header_pos + sep_len..];
            let content = content.trim_end_matches("\r\n").trim_end_matches('\n');

            let mut name: Option<String>     = None;
            let mut filename: Option<String> = None;
            let mut content_type = "text/plain".to_string();

            for line in headers_raw.lines() {
                let line = line.trim();
                if line.to_lowercase().starts_with("content-disposition:") {
                    if let Some(n) = extract_quoted(line, "name=") { name = Some(n); }
                    if let Some(f) = extract_quoted(line, "filename=") { filename = Some(f); }
                } else if line.to_lowercase().starts_with("content-type:") {
                    content_type = line[line.find(':').unwrap() + 1..].trim().to_string();
                }
            }

            let name = match name { Some(n) => n, None => continue };

            fields.push(MultipartField {
                name,
                filename,
                content_type,
                data: content.as_bytes().to_vec(),
            });
        }

        Ok(Self { fields })
    }

    /// Extract boundary from Content-Type header value.
    /// e.g. `multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW`
    pub fn boundary_from_content_type(ct: &str) -> Option<String> {
        for part in ct.split(';') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("boundary=") {
                return Some(rest.trim_matches('"').to_string());
            }
        }
        None
    }

    /// Parse from a `Request` directly.
    pub fn from_request(req: &Request) -> Result<Self, String> {
        let ct = req.headers.get("content-type")
            .ok_or_else(|| "missing Content-Type header".to_string())?;
        let boundary = Self::boundary_from_content_type(ct)
            .ok_or_else(|| "missing boundary in Content-Type".to_string())?;
        Self::parse(&req.body, &boundary)
    }

    /// Get the first field with the given name.
    pub fn field(&self, name: &str) -> Option<&MultipartField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Get the first file field with the given name.
    pub fn file(&self, name: &str) -> Option<&MultipartField> {
        self.fields.iter().find(|f| f.name == name && f.is_file())
    }

    /// All fields.
    pub fn fields(&self) -> &[MultipartField] { &self.fields }

    /// Number of fields.
    pub fn len(&self) -> usize { self.fields.len() }
    pub fn is_empty(&self) -> bool { self.fields.is_empty() }
}

/// Extract a quoted value: given `name="foo"` returns `"foo"`.
fn extract_quoted(s: &str, key: &str) -> Option<String> {
    let pos = s.find(key)?;
    let rest = &s[pos + key.len()..];
    if let Some(inner) = rest.strip_prefix('"') {
        let end = inner.find('"')?;
        Some(inner[..end].to_string())
    } else {
        let end = rest.find([';', ' ', '\r', '\n']).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDARY: &str = "----WebKitBoundary";

    fn make_body(parts: &[(&str, &str, Option<&str>, &str)]) -> Vec<u8> {
        let mut body = String::new();
        for (name, ct, filename, data) in parts {
            body.push_str(&format!("--{}\r\n", BOUNDARY));
            let disp = if let Some(f) = filename {
                format!("Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n", name, f)
            } else {
                format!("Content-Disposition: form-data; name=\"{}\"\r\n", name)
            };
            body.push_str(&disp);
            body.push_str(&format!("Content-Type: {}\r\n", ct));
            body.push_str("\r\n");
            body.push_str(data);
            body.push_str("\r\n");
        }
        body.push_str(&format!("--{}--\r\n", BOUNDARY));
        body.into_bytes()
    }

    #[test]
    fn parse_simple_text_field() {
        let body = make_body(&[("username", "text/plain", None, "alice")]);
        let form = MultipartForm::parse(&body, BOUNDARY).unwrap();
        let f = form.field("username").unwrap();
        assert_eq!(f.as_text().unwrap(), "alice");
        assert!(!f.is_file());
    }

    #[test]
    fn parse_file_field() {
        let body = make_body(&[("avatar", "image/png", Some("photo.png"), "PNG_BYTES")]);
        let form = MultipartForm::parse(&body, BOUNDARY).unwrap();
        let f = form.file("avatar").unwrap();
        assert_eq!(f.filename.as_deref(), Some("photo.png"));
        assert_eq!(f.content_type, "image/png");
        assert!(f.is_file());
    }

    #[test]
    fn parse_multiple_fields() {
        let body = make_body(&[
            ("name",  "text/plain", None, "Bob"),
            ("email", "text/plain", None, "bob@test.com"),
        ]);
        let form = MultipartForm::parse(&body, BOUNDARY).unwrap();
        assert_eq!(form.len(), 2);
        assert_eq!(form.field("email").unwrap().as_text().unwrap(), "bob@test.com");
    }

    #[test]
    fn missing_field_returns_none() {
        let body = make_body(&[("name", "text/plain", None, "Carol")]);
        let form = MultipartForm::parse(&body, BOUNDARY).unwrap();
        assert!(form.field("missing").is_none());
    }

    #[test]
    fn boundary_extraction_from_content_type() {
        let ct = "multipart/form-data; boundary=----WebKitFormBoundaryXYZ";
        assert_eq!(
            MultipartForm::boundary_from_content_type(ct).unwrap(),
            "----WebKitFormBoundaryXYZ"
        );
    }

    #[test]
    fn boundary_with_quoted_value() {
        let ct = r#"multipart/form-data; boundary="my-boundary""#;
        assert_eq!(
            MultipartForm::boundary_from_content_type(ct).unwrap(),
            "my-boundary"
        );
    }

    #[test]
    fn empty_body_returns_empty_form() {
        let form = MultipartForm::parse(b"", "boundary").unwrap();
        assert!(form.is_empty());
    }
}
