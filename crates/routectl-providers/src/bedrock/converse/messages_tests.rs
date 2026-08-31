// Host module for the Converse `messages.rs` sidecar test groups. Each
// group lives in a sibling `messages_*_tests.rs` fragment `include!`d
// below, so every test compiles into THIS module and its imports stay in
// one place -- fragments carry no `use` lines of their own.

use super::*;
use crate::anthropic_api::ANTHROPIC_FORMAT;
use routectl_core::cache_control::CacheControl;
use routectl_testkit::{CapturedEvent, capture_events};

/// Provider id passed to the translators under test; only reaches log
/// fields and the `provider` slot of a `NormalizeRequest` error.
const TEST_ID: &str = "prov-test";

include!("messages_reasoning_warn_tests.rs");
include!("messages_image_policy_tests.rs");
include!("messages_document_policy_tests.rs");
include!("messages_other_role_tests.rs");
include!("messages_tool_result_cache_control_tests.rs");
