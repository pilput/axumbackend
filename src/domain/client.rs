//! Who sent a request, as echobackend records it in `sessions`,
//! `auth_activity_logs` and `post_views`.

/// Resolved client IP (`c.RealIP()`, see `crate::http::client_ip`) and the
/// `User-Agent` header. Empty values are `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientInfo {
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
}

impl ClientInfo {
    /// Whether the request most likely comes from an automated client
    /// (crawler, link-preview fetcher, headless browser, CLI tool) rather
    /// than a person. A missing `User-Agent` counts as automated: every
    /// browser sends one. Use it to keep such traffic out of metrics; it is a
    /// heuristic, not a security boundary.
    pub fn is_likely_bot(&self) -> bool {
        self.user_agent.as_deref().is_none_or(is_bot_user_agent)
    }
}

/// Lowercase `User-Agent` fragments of automated clients. Extend this list
/// when new crawlers show up in `post_views`.
const BOT_USER_AGENT_MARKERS: &[&str] = &[
    // generic crawler words (Googlebot, bingbot, AhrefsBot, Slurp, ...)
    "bot",
    "crawler",
    "spider",
    "slurp",
    // link previews (Slack, Discord, WhatsApp, Facebook, embed services)
    "preview",
    "facebookexternalhit",
    "embedly",
    // headless browsers and auditing tools
    "headless",
    "lighthouse",
    // CLI and library HTTP clients
    "curl/",
    "wget/",
    "python-requests",
    "httpclient",
];

fn is_bot_user_agent(user_agent: &str) -> bool {
    let user_agent = user_agent.to_ascii_lowercase();
    BOT_USER_AGENT_MARKERS
        .iter()
        .any(|marker| user_agent.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(user_agent: Option<&str>) -> ClientInfo {
        ClientInfo {
            ip_address: Some("203.0.113.5".into()),
            user_agent: user_agent.map(Into::into),
        }
    }

    #[test]
    fn detects_automated_clients() {
        for ua in [
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "facebookexternalhit/1.1",
            "Mozilla/5.0 HeadlessChrome/120.0",
            "curl/8.5.0",
            "python-requests/2.32",
        ] {
            assert!(client(Some(ua)).is_likely_bot(), "{ua}");
        }
        assert!(client(None).is_likely_bot());
    }

    #[test]
    fn browsers_are_not_bots() {
        for ua in [
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/131.0 Safari/537.36",
            "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) Version/18.0 Mobile Safari/604.1",
        ] {
            assert!(!client(Some(ua)).is_likely_bot(), "{ua}");
        }
    }
}
