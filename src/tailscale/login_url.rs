use std::fmt;

use thiserror::Error;
use url::Url;

const LOGIN_URL_PREFIX: &str = "https://login.tailscale.com/";
const MAX_LOGIN_URL_BYTES: usize = 4 * 1024;

/// A validated, bounded Tailscale CLI authentication URL.
#[derive(Clone, Eq, PartialEq)]
pub struct LoginUrl(Url);

impl LoginUrl {
    pub fn parse(candidate: &str) -> Result<Self, LoginUrlError> {
        if candidate.len() > MAX_LOGIN_URL_BYTES {
            return Err(LoginUrlError::TooLong);
        }
        let parsed = Url::parse(candidate).map_err(LoginUrlError::InvalidUrl)?;
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.host_str() != Some("login.tailscale.com")
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(LoginUrlError::UntrustedOrigin);
        }
        let mut segments = parsed.path_segments().ok_or(LoginUrlError::InvalidPath)?;
        let valid_path = matches!(segments.next(), Some("a"))
            && segments.next().is_some_and(|token| !token.is_empty())
            && segments.next().is_none();
        if !valid_path {
            return Err(LoginUrlError::InvalidPath);
        }
        Ok(Self(parsed))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for LoginUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("LoginUrl").field(&"[redacted]").finish()
    }
}

impl fmt::Display for LoginUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Error)]
pub enum LoginUrlError {
    #[error("Tailscale login URL exceeds {MAX_LOGIN_URL_BYTES} bytes")]
    TooLong,
    #[error("invalid URL: {0}")]
    InvalidUrl(#[source] url::ParseError),
    #[error("URL is not the trusted Tailscale login origin")]
    UntrustedOrigin,
    #[error("URL is not a Tailscale CLI authentication path")]
    InvalidPath,
}

/// Finds the first valid Tailscale CLI authentication URL in bounded process output.
pub fn find_login_url(output: &str) -> Option<LoginUrl> {
    let mut search = output;
    while let Some(start) = search.find(LOGIN_URL_PREFIX) {
        let candidate = search[start..]
            .chars()
            .take_while(|character| !character.is_whitespace() && !character.is_control())
            .take(MAX_LOGIN_URL_BYTES + 1)
            .collect::<String>();
        let candidate = candidate.trim_end_matches([')', ']', '}', ',', ';', '.', '!', '?']);
        if let Ok(url) = LoginUrl::parse(candidate) {
            return Some(url);
        }
        search = &search[start + LOGIN_URL_PREFIX.len()..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_strict_cli_authentication_urls() {
        assert_eq!(
            find_login_url(
                "ignore https://login.tailscale.com.evil/a/nope then use \
                 https://login.tailscale.com/a/real-token,"
            )
            .as_ref()
            .map(LoginUrl::as_str),
            Some("https://login.tailscale.com/a/real-token")
        );
        for rejected in [
            "http://login.tailscale.com/a/token",
            "https://login.tailscale.com.evil/a/token",
            "https://user@login.tailscale.com/a/token",
            "https://login.tailscale.com:444/a/token",
            "https://login.tailscale.com/admin",
            "https://login.tailscale.com/a/token/extra",
            "https://login.tailscale.com/a/token?next=evil",
            "https://login.tailscale.com/a/token#fragment",
        ] {
            assert!(find_login_url(rejected).is_none(), "accepted {rejected}");
        }
    }
}
