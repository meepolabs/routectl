//! `routectl login <provider>` -- stub for v0.1. Real cookie capture lands
//! in v0.2 with the `wry` webview popup and ToS-on-user disclosure.

use routectl_core::{Error, Result};

pub fn run(provider: &str) -> Result<()> {
    Err(Error::Auth(format!(
        "`routectl login {provider}` is not enabled in this build. \
         Cookie-auth providers (claude.ai, chatgpt.com) ship in v0.2 \
         once the consumer-session capture flow + ToS-on-user surface land."
    )))
}
