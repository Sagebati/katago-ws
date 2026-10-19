//! Shared-secret Bearer checks: the cluster socket's worker auth and the
//! trusted-client token on `POST /analyse/batch` use the same comparison.

use axum::http::{HeaderMap, header};

/// The token in an `Authorization: Bearer <token>` header, if there is one.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Whether `headers` carry `expected` as their Bearer token. Compared in constant
/// time; an empty `expected` never matches, so an unset secret can't be
/// satisfied by an empty header.
pub fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    !expected.is_empty()
        && bearer(headers).is_some_and(|presented| tokens_match(presented, expected))
}

/// Length-checked constant-time token comparison (avoids leaking match length via
/// early return on the byte loop).
pub fn tokens_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in presented.iter().zip(expected) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn constant_time_compare_matches_only_identical() {
        assert!(tokens_match("s3cret", "s3cret"));
        assert!(!tokens_match("s3cret", "s3crew"));
        assert!(!tokens_match("s3cret", "s3cre")); // length differs
    }

    #[test]
    fn bearer_matches_only_the_expected_token() {
        assert!(bearer_matches(&with_bearer("topsecret"), "topsecret"));
        assert!(!bearer_matches(&with_bearer("wrong"), "topsecret"));
        assert!(!bearer_matches(&HeaderMap::new(), "topsecret"));
    }

    #[test]
    fn an_empty_secret_matches_nothing() {
        assert!(!bearer_matches(&with_bearer(""), ""));
        assert!(!bearer_matches(&HeaderMap::new(), ""));
    }
}
