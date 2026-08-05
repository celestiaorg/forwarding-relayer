use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result};
use tonic::metadata::AsciiMetadataValue;

/// One configured endpoint: its URL and an optional per-endpoint auth token.
pub struct EndpointSpec {
    pub url: String,
    pub token: Option<AuthToken>,
}

/// Parse a comma-separated list of `url|token` (or bare `url`) endpoint specs, e.g.
/// `http://a:9090|tokA,http://b:9090`. Each entry's token is optional (no `|`, or an
/// empty `url|` → no token) and is validated with [`AuthToken`]. Whitespace around
/// entries, URLs, and tokens is trimmed; empty entries are skipped.
pub fn parse_endpoint_specs(spec: &str) -> Result<Vec<EndpointSpec>> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            let (url, token) = entry
                .split_once('|')
                .map(|(u, t)| (u.trim(), t.trim()))
                .unwrap_or((entry, ""));
            let token = if token.is_empty() {
                None
            } else {
                Some(
                    token
                        .parse::<AuthToken>()
                        .map_err(|e| anyhow::anyhow!(e))
                        .with_context(|| format!("invalid auth token for endpoint {url}"))?,
                )
            };
            Ok(EndpointSpec {
                url: url.to_string(),
                token,
            })
        })
        .collect()
}

/// Render an endpoint-spec list for logging with the `|token` parts stripped, so a
/// raw `CELESTIA_GRPC`/`CELESTIA_RPC` value never leaks its tokens.
pub fn redact_endpoint_specs(spec: &str) -> String {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            entry
                .split_once('|')
                .map(|(u, _)| u.trim())
                .unwrap_or(entry)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A gateway auth token restricted to URL-safe characters.
///
/// The invariant is enforced by construction ([`FromStr`]), so downstream code can
/// never receive an invalid token.
#[derive(Clone, Debug)]
pub struct AuthToken(String);

impl AuthToken {
    /// The token as a string, for the RPC URL splice and the lumina tx endpoint.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The token as a gRPC metadata value for the tonic query interceptor.
    ///
    /// Infallible: the URL-safe character set is a subset of valid ASCII header
    /// values, guaranteed by the constructor.
    pub fn metadata_value(&self) -> AsciiMetadataValue {
        AsciiMetadataValue::from_str(&self.0)
            .expect("a URL-safe AuthToken is always a valid ASCII metadata value")
    }
}

impl FromStr for AuthToken {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            return Err("must not be empty".into());
        }
        if let Some(bad) = s
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~')))
        {
            return Err(format!(
                "contains unsupported character {bad:?}; only URL-safe characters are \
                 allowed (ASCII alphanumerics and - _ . ~)"
            ));
        }
        Ok(AuthToken(s.to_string()))
    }
}

impl fmt::Display for AuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_url_safe_charset() {
        for token in ["abcdef0123456789", "A-z_0.9~", "deadBEEF"] {
            assert!(
                token.parse::<AuthToken>().is_ok(),
                "{token} should be valid"
            );
        }
    }

    #[test]
    fn rejects_url_breaking_chars_and_empty() {
        // These pass a plain ASCII-header check but corrupt the RPC URL splice.
        for token in ["ab/cd", "ab@cd", "ab cd", "a:b", "tok+en", "tok=en", ""] {
            assert!(
                token.parse::<AuthToken>().is_err(),
                "{token:?} should be rejected"
            );
        }
    }

    #[test]
    fn metadata_value_matches_token() {
        let token: AuthToken = "abc123".parse().unwrap();
        assert_eq!(token.metadata_value().to_str().unwrap(), "abc123");
    }

    #[test]
    fn parses_per_endpoint_url_token_pairs() {
        let specs =
            parse_endpoint_specs(" http://a:9090|tokA , http://b:9090 , http://c:9090| ").unwrap();
        let got: Vec<_> = specs
            .iter()
            .map(|s| (s.url.as_str(), s.token.as_ref().map(AuthToken::as_str)))
            .collect();
        assert_eq!(
            got,
            vec![
                ("http://a:9090", Some("tokA")), // url|token
                ("http://b:9090", None),         // bare url
                ("http://c:9090", None),         // empty token after '|'
            ]
        );
    }

    #[test]
    fn rejects_invalid_token_in_a_pair() {
        assert!(parse_endpoint_specs("http://a:9090|bad/token").is_err());
    }

    #[test]
    fn empty_spec_yields_no_endpoints() {
        assert!(parse_endpoint_specs(" , ").unwrap().is_empty());
    }

    #[test]
    fn redact_strips_tokens_for_logging() {
        assert_eq!(
            redact_endpoint_specs("http://a:9090|tokA, http://b:9090, http://c:9090|tokC"),
            "http://a:9090, http://b:9090, http://c:9090"
        );
    }
}
