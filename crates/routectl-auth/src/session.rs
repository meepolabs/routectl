use routectl_core::Result;

/// Cookie-session capture surface. One implementation per upstream
/// (claude.ai, chatgpt.com); login flow lives in routectl-cli.
/// Deferred to v0.2.
pub trait SessionCapture {
    fn login_url(&self) -> &str;
    fn capture(&self, cookies: &[Cookie]) -> Result<CapturedSession>;
}

#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CapturedSession {
    pub provider: String,
    pub cookies: Vec<Cookie>,
    pub captured_at: i64,
}
