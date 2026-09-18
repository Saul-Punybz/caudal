//! Passwords (argon2id PHC strings) and API tokens (SHA-256).

use std::sync::OnceLock;

use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use argon2::{Algorithm, Argon2};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Hashes a password with argon2id and the crate's default (OWASP) cost,
/// a fresh random salt, as a PHC string (`$argon2id$v=19$m=...`).
pub fn hash_password(password: &str) -> Result<String, String> {
    Argon2::default().hash_password(password.as_bytes()).map(|h| h.to_string()).map_err(|e| e.to_string())
}

/// Rejects anything but a well-formed argon2id PHC string.
pub(crate) fn check_phc(phc: &str) -> Result<(), String> {
    let parsed = PasswordHash::new(phc).map_err(|_| "is not a PHC string (use `caudal hash-password`)".to_string())?;
    if parsed.algorithm.as_str() != Algorithm::Argon2id.ident().as_str() {
        return Err("must be argon2id".into());
    }
    Ok(())
}

/// Checks `password` against a PHC string. argon2's own comparison is
/// constant-time; the cost comes from the parameters inside `phc`.
pub fn verify_password(phc: &str, password: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

/// Burns the same time as a real check, for names that do not exist, so
/// response time does not reveal which user names are real.
pub(crate) fn verify_dummy(password: &str) {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let phc = DUMMY.get_or_init(|| hash_password("caudal-dummy-password").unwrap_or_default());
    let _ = verify_password(phc, password);
}

/// SHA-256 of a presented bearer token.
pub(crate) fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Whether `token` hashes to one of `digests`. Every digest is compared,
/// each in constant time, so neither the match position nor its prefix
/// leaks.
pub(crate) fn match_token<'a>(token: &str, digests: &'a [(String, [u8; 32])]) -> Option<&'a str> {
    let presented = token_digest(token);
    let mut found = None;
    for (name, d) in digests {
        if bool::from(presented.ct_eq(d)) {
            found = Some(name.as_str());
        }
    }
    found
}

/// `len` random bytes from the OS, base64url without padding.
pub(crate) fn random_token(len: usize) -> String {
    use base64::Engine;
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).expect("OS random number generator");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify() {
        let phc = hash_password("correct horse").unwrap();
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        check_phc(&phc).unwrap();
        assert!(verify_password(&phc, "correct horse"));
        assert!(!verify_password(&phc, "correct horse "));
        assert!(!verify_password("not a hash", "x"));
        // Same password, fresh salt: different strings.
        assert_ne!(phc, hash_password("correct horse").unwrap());
    }

    #[test]
    fn only_argon2id_is_accepted() {
        let argon2i = "$argon2i$v=19$m=16,t=2,p=1$c29tZXNhbHQ$bnc2kvlbJ7g3mUpbIlSwBQ";
        assert!(check_phc(argon2i).unwrap_err().contains("argon2id"));
        assert!(check_phc("$2y$10$abcdefghijklmnopqrstuv").is_err());
    }

    #[test]
    fn tokens_match_by_digest() {
        let digests = vec![("ci".to_string(), token_digest("s3cret")), ("prom".to_string(), token_digest("other"))];
        assert_eq!(match_token("s3cret", &digests), Some("ci"));
        assert_eq!(match_token("other", &digests), Some("prom"));
        assert_eq!(match_token("s3cre", &digests), None);
        assert_eq!(match_token("", &digests), None);
    }

    #[test]
    fn random_tokens_are_256_bit_and_distinct() {
        let a = random_token(32);
        assert_eq!(a.len(), 43);
        assert_ne!(a, random_token(32));
    }
}
