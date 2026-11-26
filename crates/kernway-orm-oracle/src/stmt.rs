//! Client-side SQL classification and bind-placeholder discovery (the server
//! needs only bind positions; names never go on the wire).

/// What kind of statement the SQL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Query,
    Dml,
    Ddl,
    PlSql,
    Other,
}

/// Classify by the first keyword.
pub(crate) fn classify(sql: &str) -> Kind {
    let body = skip_leading_noise(sql);
    let word: String = body.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    match word.as_str() {
        "SELECT" | "WITH" => Kind::Query,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => Kind::Dml,
        "DECLARE" | "BEGIN" | "CALL" => Kind::PlSql,
        "CREATE" | "ALTER" | "DROP" | "GRANT" | "REVOKE" | "ANALYZE" | "AUDIT" | "COMMENT" | "TRUNCATE" => {
            Kind::Ddl
        }
        _ => Kind::Other,
    }
}

fn skip_leading_noise(mut s: &str) -> &str {
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = t.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, r)| r);
        } else if let Some(rest) = t.strip_prefix('(') {
            s = rest;
        } else {
            return t;
        }
    }
}

/// Bind placeholder names in wire order. PL/SQL lists a repeated name once;
/// SQL lists every occurrence. Names are upper-cased unless quoted.
pub(crate) fn bind_names(sql: &str, kind: Kind) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if kind == Kind::Ddl {
        return names;
    }
    let b = sql.as_bytes();
    let mut i = 0;
    let mut prev_quote = false;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
                prev_quote = true;
                continue;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b':' if !prev_quote && b.get(i + 1) != Some(&b'=') => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let name = if b.get(j) == Some(&b'"') {
                    let start = j + 1;
                    let end = sql[start..].find('"').map_or(b.len(), |k| start + k);
                    j = end + 1;
                    Some(sql[start..end].to_string())
                } else {
                    let start = j;
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'$' | b'#')) {
                        j += 1;
                    }
                    (j > start && (b[start].is_ascii_alphabetic() || b[start].is_ascii_digit()))
                        .then(|| sql[start..j].to_ascii_uppercase())
                };
                if let Some(n) = name {
                    if kind != Kind::PlSql || !names.contains(&n) {
                        names.push(n);
                    }
                    i = j;
                    prev_quote = false;
                    continue;
                }
                i += 1;
            }
            _ => i += 1,
        }
        prev_quote = false;
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_first_keyword() {
        assert_eq!(classify("  select 1 from dual"), Kind::Query);
        assert_eq!(classify("WITH x AS (SELECT 1 FROM dual) SELECT * FROM x"), Kind::Query);
        assert_eq!(classify("-- c\n insert into t values (1)"), Kind::Dml);
        assert_eq!(classify("/* c */ BEGIN NULL; END;"), Kind::PlSql);
        assert_eq!(classify("create table t (x number)"), Kind::Ddl);
    }

    #[test]
    fn finds_binds_and_skips_literals_comments_and_assignment() {
        let k = Kind::Query;
        assert_eq!(
            bind_names("SELECT ':no', \"a:b\" FROM t WHERE a = :id AND b = : name -- :c\n AND c = :id /* :d */", k),
            ["ID", "NAME", "ID"]
        );
        assert_eq!(
            bind_names("BEGIN :r := f(:a, :a); :\"Mixed\" := 1; END;", Kind::PlSql),
            ["R", "A", "Mixed"]
        );
        assert!(bind_names("CREATE TABLE t (x VARCHAR2(10) DEFAULT ':x')", Kind::Ddl).is_empty());
        assert_eq!(bind_names("select :1, :2 from dual", k), ["1", "2"]);
    }
}
