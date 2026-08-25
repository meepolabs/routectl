// Host module for the `messages.rs` sidecar test groups. Each group lives
// in a sibling `messages_*_tests.rs` fragment `include!`d below, so every
// test compiles into THIS module and its imports stay in one place --
// fragments carry no `use` lines of their own.

use super::*;
use crate::anthropic_api::ANTHROPIC_FORMAT;
use crate::anthropic_api::envelope_policy::passthrough_tally;
use crate::bounded_diagnostics::MAX_LOGGED_DIAGNOSTIC_ITEMS;
use routectl_testkit::{CapturedEvent, capture_events};

include!("messages_reasoning_warn_tests.rs");
include!("messages_replay_invariant_warn_tests.rs");
include!("messages_system_turn_tests.rs");
include!("messages_thinking_display_tests.rs");
