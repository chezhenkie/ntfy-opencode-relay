//! Request, response, and error shapes for the control endpoint.

use serde::{Deserialize, Serialize};

/// Network mode name accepted by the deploy helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Tailscale,
    Nord,
    Lan,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tailscale" => Some(Self::Tailscale),
            "nord" => Some(Self::Nord),
            "lan" => Some(Self::Lan),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tailscale => "tailscale",
            Self::Nord => "nord",
            Self::Lan => "lan",
        }
    }
}

/// Failure classes shared by the endpoint and the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Invalid,
    NotFound,
    Conflict,
    Covered,
    Unsupported,
    Internal,
    Unauthorized,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotFound => "not-found",
            Self::Conflict => "conflict",
            Self::Covered => "covered",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
            Self::Unauthorized => "unauthorized",
        }
    }

    pub fn status(self) -> u16 {
        match self {
            Self::Invalid => 400,
            Self::NotFound => 404,
            Self::Conflict | Self::Covered => 409,
            Self::Unsupported => 501,
            Self::Internal => 500,
            Self::Unauthorized => 401,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PathRequest {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct ModeRequest {
    pub mode: String,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicsData {
    pub roots: Vec<String>,
    pub topics: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusData {
    pub pid: u32,
    pub started: String,
    pub roots: Vec<String>,
    pub topics: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModePlanData {
    pub mode: Mode,
    pub deploy: String,
    pub command: Vec<String>,
}

/// One success body, ready to write as an HTTP response.
pub fn ok_json<T: Serialize>(data: T) -> (u16, String) {
    (
        200,
        serde_json::json!({"ok": true, "data": data}).to_string(),
    )
}

/// One failure body, ready to write as an HTTP response.
pub fn error_json(code: ErrorCode, message: &str) -> (u16, String) {
    (
        code.status(),
        serde_json::json!({"ok": false, "error": {"code": code.as_str(), "message": message}})
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_round_trip() {
        for (text, mode) in [
            ("tailscale", Mode::Tailscale),
            ("nord", Mode::Nord),
            ("lan", Mode::Lan),
        ] {
            assert_eq!(Mode::parse(text), Some(mode));
            assert_eq!(mode.as_str(), text);
        }
        assert_eq!(Mode::parse("vpn"), None);
    }

    #[test]
    fn error_codes_map_to_status() {
        assert_eq!(ErrorCode::Invalid.status(), 400);
        assert_eq!(ErrorCode::Unauthorized.status(), 401);
        assert_eq!(ErrorCode::NotFound.status(), 404);
        assert_eq!(ErrorCode::Covered.status(), 409);
        assert_eq!(ErrorCode::Internal.status(), 500);
        assert_eq!(ErrorCode::Unsupported.status(), 501);
    }

    #[test]
    fn bodies_carry_the_envelope() {
        let (status, body) = ok_json(TopicsData {
            roots: vec!["/srv".into()],
            topics: vec!["app".into()],
        });
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["roots"][0], "/srv");
        assert_eq!(value["data"]["topics"][0], "app");

        let (status, body) = error_json(ErrorCode::Covered, "inside a root");
        assert_eq!(status, 409);
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "covered");
        assert_eq!(value["error"]["message"], "inside a root");
    }
}
