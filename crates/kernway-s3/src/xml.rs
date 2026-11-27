//! Just enough XML for S3's responses: elements by name (namespaces and
//! attributes ignored), their text with entities decoded.

/// The inner text of every `<name>…</name>` directly or deeply inside `xml`,
/// in order. Self-closing `<name/>` counts as empty.
pub fn all<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        // The tag must end here: `>`, `/>`, or whitespace before attributes.
        let Some(c) = after.chars().next() else { break };
        if !(c == '>' || c == '/' || c.is_whitespace()) {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        if after[..gt].ends_with('/') {
            out.push("");
            rest = &after[gt + 1..];
            continue;
        }
        let body = &after[gt + 1..];
        match body.find(&close) {
            Some(j) => {
                out.push(&body[..j]);
                rest = &body[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// The first `<name>` element's inner text.
pub fn first<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    all(xml, name).into_iter().next()
}

/// Decode `&amp; &lt; &gt; &quot; &apos; &#N; &#xN;`.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let ent = &after[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text of `<name>` inside `xml`, decoded.
pub fn text(xml: &str, name: &str) -> Option<String> {
    first(xml, name).map(unescape)
}

/// Escape text for an XML body.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_and_entities() {
        let x = r#"<?xml version="1.0"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name>
            <Contents><Key>a &amp; b.txt</Key><Size>3</Size></Contents><Contents><Key>&#7843;nh.jpg</Key><Size>0</Size></Contents>
            <KeyCount>2</KeyCount><IsTruncated>false</IsTruncated><Prefix/></ListBucketResult>"#;
        let c = all(x, "Contents");
        assert_eq!(c.len(), 2);
        assert_eq!(text(c[0], "Key").as_deref(), Some("a & b.txt"));
        assert_eq!(text(c[1], "Key").as_deref(), Some("ảnh.jpg"));
        assert_eq!(text(x, "KeyCount").as_deref(), Some("2"), "not confused with <Key>");
        assert_eq!(first(x, "Prefix"), Some(""), "self-closing");
        assert_eq!(first(x, "Missing"), None);
        assert_eq!(unescape("x &bogus; y &lt;"), "x &bogus; y <");
        assert_eq!(escape("a<&\"'>"), "a&lt;&amp;&quot;&apos;&gt;");
    }
}
