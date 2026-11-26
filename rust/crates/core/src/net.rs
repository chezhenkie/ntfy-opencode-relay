//! URL rules shared by the ntfy server and the OpenCode base URL.
//!
//! Plain HTTP is allowed only to this machine; every other host is TLS. The
//! same host check serves the ntfy client, the relay config and the service
//! registration, so one rule decides every outbound address.

use std::fmt;

use url::{Host, Url};

/// True when the URL points at this machine.
pub fn host_is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// HTTPS anywhere, plain HTTP only on a loopback host.
pub fn transport_allowed(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => host_is_loopback(url),
        _ => false,
    }
}

/// A parsed base URL: transport allowed, no path, normalized without a
/// trailing slash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    base: String,
}

impl BaseUrl {
    pub fn parse(raw: &str) -> Result<Self, BaseUrlError> {
        if raw.is_empty() {
            return Err(BaseUrlError::Empty);
        }
        if raw.chars().any(|c| c.is_whitespace() || c == '\u{feff}') {
            return Err(BaseUrlError::Whitespace);
        }
        let url = Url::parse(raw).map_err(|_| BaseUrlError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(BaseUrlError::UnsupportedScheme);
        }
        if url.host_str().is_none() {
            return Err(BaseUrlError::MissingHost);
        }
        if !transport_allowed(&url) {
            return Err(BaseUrlError::PlainHttp);
        }
        if !url.path().is_empty() && url.path() != "/" {
            return Err(BaseUrlError::Path);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(BaseUrlError::QueryOrFragment);
        }
        if !url.username().is_empty() || url.password().is_some() || has_credentials(raw) {
            return Err(BaseUrlError::Credentials);
        }
        Ok(Self {
            base: raw.trim_end_matches('/').to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.base
    }

    /// Joins an absolute API path. `path` must start with `/` and is assumed
    /// already encoded.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

impl fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseUrlError {
    Empty,
    Whitespace,
    InvalidUrl,
    UnsupportedScheme,
    MissingHost,
    /// Plain HTTP to a host that is not this machine.
    PlainHttp,
    /// A path after the authority; the base must be the origin only.
    Path,
    QueryOrFragment,
    Credentials,
}

impl fmt::Display for BaseUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::Empty => "base URL must not be empty",
            Self::Whitespace => "base URL must not contain whitespace",
            Self::InvalidUrl => "base URL is not a valid URL",
            Self::UnsupportedScheme => "base URL must start with http:// or https://",
            Self::MissingHost => "base URL must include a host",
            Self::PlainHttp => "base URL must use HTTPS unless the host is loopback",
            Self::Path => "base URL must not carry a path",
            Self::QueryOrFragment => "base URL must not contain a query or fragment",
            Self::Credentials => "base URL must not contain credentials",
        };
        f.write_str(detail)
    }
}

impl std::error::Error for BaseUrlError {}

fn has_credentials(raw: &str) -> bool {
    let Some(scheme_end) = raw.find("://") else {
        return false;
    };
    let authority = &raw[scheme_end + 3..];
    let end = authority.find('/').unwrap_or(authority.len());
    authority[..end].contains('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_allowed_on_any_host() {
        assert!(BaseUrl::parse("https://oc.example").is_ok());
        assert!(BaseUrl::parse("https://192.168.1.20:4096").is_ok());
    }

    #[test]
    fn http_is_allowed_only_on_loopback() {
        for loopback in [
            "http://127.0.0.1:4096",
            "http://[::1]:4096",
            "http://localhost:4096",
        ] {
            assert!(BaseUrl::parse(loopback).is_ok(), "{loopback}");
        }
        assert_eq!(
            BaseUrl::parse("http://192.168.1.20:4096"),
            Err(BaseUrlError::PlainHttp)
        );
        assert_eq!(
            BaseUrl::parse("http://oc.example:4096"),
            Err(BaseUrlError::PlainHttp)
        );
    }

    #[test]
    fn a_path_after_the_authority_is_refused() {
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1:4096/api"),
            Err(BaseUrlError::Path)
        );
        assert_eq!(
            BaseUrl::parse("https://oc.example/anything"),
            Err(BaseUrlError::Path)
        );
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1:4096/").unwrap().as_str(),
            "http://127.0.0.1:4096"
        );
    }

    #[test]
    fn the_existing_shape_checks_stay() {
        assert_eq!(BaseUrl::parse(""), Err(BaseUrlError::Empty));
        assert_eq!(
            BaseUrl::parse("ftp://127.0.0.1:4096"),
            Err(BaseUrlError::UnsupportedScheme)
        );
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1:4096?x=1"),
            Err(BaseUrlError::QueryOrFragment)
        );
        assert_eq!(
            BaseUrl::parse("http://opencode:pw@127.0.0.1:4096"),
            Err(BaseUrlError::Credentials)
        );
        assert_eq!(
            BaseUrl::parse("http://127.0.0.1 :4096"),
            Err(BaseUrlError::Whitespace)
        );
    }

    #[test]
    fn the_loopback_predicate_covers_ipv4_ipv6_and_localhost() {
        for raw in ["http://127.0.0.1:1", "http://[::1]:1", "http://LOCALHOST:1"] {
            let url = Url::parse(raw).unwrap();
            assert!(host_is_loopback(&url), "{raw}");
            assert!(transport_allowed(&url), "{raw}");
        }
        let url = Url::parse("http://192.168.1.20:1").unwrap();
        assert!(!host_is_loopback(&url));
        assert!(!transport_allowed(&url));
        let url = Url::parse("https://192.168.1.20:1").unwrap();
        assert!(!host_is_loopback(&url));
        assert!(transport_allowed(&url));
    }
}
