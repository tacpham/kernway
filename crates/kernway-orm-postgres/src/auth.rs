//! PostgreSQL's own password hash. SCRAM-SHA-256 (with `-PLUS` channel
//! binding) is the shared client in `kernway_wire::scram`.

use md5::Digest;

/// `md5` + hex(md5(hex(md5(password + user)) + salt)), as `AuthenticationMD5Password` wants.
pub fn md5_password(user: &str, password: &str, salt: [u8; 4]) -> String {
    let inner = hex(&md5::Md5::digest([password.as_bytes(), user.as_bytes()].concat()));
    let outer = hex(&md5::Md5::digest([inner.as_bytes(), &salt].concat()));
    format!("md5{outer}")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_postgres() {
        // SELECT 'md5' || md5(md5('secret' || 'alice') || 'abcd')
        assert_eq!(md5_password("alice", "secret", *b"abcd"), "md5c90bcafc0a2b54511742383e34d5dc9d");
    }
}
