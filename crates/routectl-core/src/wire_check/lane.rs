//! Lanes: which ingress dialect fed an attempt and which egress carried it.

use crate::RequestProvenance;

/// The HTTP ingress dialect a request arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IngressDialect {
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI Chat Completions.
    Chat,
    /// OpenAI Responses.
    Responses,
}

impl IngressDialect {
    /// Every ingress dialect, in declaration order.
    pub const ALL: [Self; 3] = [Self::Anthropic, Self::Chat, Self::Responses];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }

    /// The dialect of the ingress that parsed a request of `provenance`;
    /// none for a request built without an ingress.
    pub const fn of_provenance(provenance: RequestProvenance) -> Option<Self> {
        match provenance {
            RequestProvenance::AnthropicIngress => Some(Self::Anthropic),
            RequestProvenance::OpenaiIngress => Some(Self::Chat),
            RequestProvenance::ResponsesIngress => Some(Self::Responses),
            RequestProvenance::Library => None,
        }
    }
}

/// The wire surface an attempt egressed through. `Local` is a request
/// routectl answered or refused without sending it upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EgressKind {
    /// Anthropic Messages API, direct.
    AnthropicApi,
    /// Anthropic Messages API through Bedrock's managed surface.
    AnthropicApiMantle,
    /// Bedrock InvokeModel (Anthropic Messages body).
    BedrockInvoke,
    /// Bedrock Converse.
    BedrockConverse,
    /// OpenAI-compatible Chat Completions.
    OpenaiCompat,
    /// OpenAI-compatible Chat Completions through Bedrock's managed surface.
    OpenaiCompatMantle,
    /// OpenAI Responses.
    OpenaiResponses,
    /// OpenAI Responses through Bedrock's managed surface.
    OpenaiResponsesMantle,
    /// Gemini.
    Gemini,
    /// Answered or refused locally; nothing was sent upstream.
    Local,
}

impl EgressKind {
    /// Every egress kind, in declaration order.
    pub const ALL: [Self; 10] = [
        Self::AnthropicApi,
        Self::AnthropicApiMantle,
        Self::BedrockInvoke,
        Self::BedrockConverse,
        Self::OpenaiCompat,
        Self::OpenaiCompatMantle,
        Self::OpenaiResponses,
        Self::OpenaiResponsesMantle,
        Self::Gemini,
        Self::Local,
    ];

    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::AnthropicApi => "anthropic-api",
            Self::AnthropicApiMantle => "anthropic-api-mantle",
            Self::BedrockInvoke => "bedrock-invoke",
            Self::BedrockConverse => "bedrock-converse",
            Self::OpenaiCompat => "openai-compat",
            Self::OpenaiCompatMantle => "openai-compat-mantle",
            Self::OpenaiResponses => "openai-responses",
            Self::OpenaiResponsesMantle => "openai-responses-mantle",
            Self::Gemini => "gemini",
            Self::Local => "local",
        }
    }

    /// The ingress dialect whose body this egress sends, or `None` when no
    /// ingress speaks its wire shape.
    pub const fn wire_dialect(self) -> Option<IngressDialect> {
        match self {
            Self::AnthropicApi | Self::AnthropicApiMantle | Self::BedrockInvoke => {
                Some(IngressDialect::Anthropic)
            }
            Self::OpenaiCompat | Self::OpenaiCompatMantle => Some(IngressDialect::Chat),
            Self::OpenaiResponses | Self::OpenaiResponsesMantle => Some(IngressDialect::Responses),
            Self::BedrockConverse | Self::Gemini | Self::Local => None,
        }
    }
}

/// Whether the egress body can be compared with the ingress body key for
/// key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneClass {
    /// Ingress and egress speak the same dialect; every difference is
    /// either a declared edit or a loss.
    Fidelity,
    /// The egress translates into another dialect.
    Translation,
}

impl LaneClass {
    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fidelity => "fidelity",
            Self::Translation => "translation",
        }
    }
}

/// Direction of a body on an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Leg {
    /// Client to upstream.
    Request,
    /// Upstream to client.
    Response,
}

impl Leg {
    /// Static label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
        }
    }
}

/// One (ingress dialect, egress kind) pair and its derived class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Lane {
    ingress: IngressDialect,
    egress: EgressKind,
    class: LaneClass,
}

macro_rules! lane_label {
    ($ingress:literal, $egress:expr) => {
        match $egress {
            EgressKind::AnthropicApi => concat!($ingress, "->anthropic-api"),
            EgressKind::AnthropicApiMantle => concat!($ingress, "->anthropic-api-mantle"),
            EgressKind::BedrockInvoke => concat!($ingress, "->bedrock-invoke"),
            EgressKind::BedrockConverse => concat!($ingress, "->bedrock-converse"),
            EgressKind::OpenaiCompat => concat!($ingress, "->openai-compat"),
            EgressKind::OpenaiCompatMantle => concat!($ingress, "->openai-compat-mantle"),
            EgressKind::OpenaiResponses => concat!($ingress, "->openai-responses"),
            EgressKind::OpenaiResponsesMantle => concat!($ingress, "->openai-responses-mantle"),
            EgressKind::Gemini => concat!($ingress, "->gemini"),
            EgressKind::Local => concat!($ingress, "->local"),
        }
    };
}

impl Lane {
    /// Number of distinct lanes: every ingress dialect times every egress
    /// kind.
    pub const COUNT: usize = IngressDialect::ALL.len() * EgressKind::ALL.len();

    /// Dense index of this lane in `0..Lane::COUNT`, unique per
    /// (ingress, egress) pair, for fixed per-lane tables.
    pub const fn index(self) -> usize {
        self.ingress as usize * EgressKind::ALL.len() + self.egress as usize
    }

    /// The lane for `ingress` feeding `egress`: FIDELITY exactly when the
    /// egress sends the ingress's own dialect.
    pub fn of(ingress: IngressDialect, egress: EgressKind) -> Self {
        let class = if egress_speaks(ingress, egress) {
            LaneClass::Fidelity
        } else {
            LaneClass::Translation
        };
        Self {
            ingress,
            egress,
            class,
        }
    }

    /// The ingress dialect.
    pub const fn ingress(self) -> IngressDialect {
        self.ingress
    }

    /// The egress kind.
    pub const fn egress(self) -> EgressKind {
        self.egress
    }

    /// FIDELITY or TRANSLATION.
    pub const fn class(self) -> LaneClass {
        self.class
    }

    /// Static `<ingress>-><egress>` label for logs and counters.
    pub const fn label(self) -> &'static str {
        match self.ingress {
            IngressDialect::Anthropic => lane_label!("anthropic", self.egress),
            IngressDialect::Chat => lane_label!("chat", self.egress),
            IngressDialect::Responses => lane_label!("responses", self.egress),
        }
    }
}

/// Whether `egress` sends the wire dialect `ingress` parsed.
fn egress_speaks(ingress: IngressDialect, egress: EgressKind) -> bool {
    egress.wire_dialect() == Some(ingress)
}

/// Whether members routectl does not model may forward unchanged from a
/// request of `provenance` to `egress`: the request was built by a library
/// caller, or its ingress dialect is the dialect `egress` sends. For an
/// ingress request this is exactly the FIDELITY class of its [`Lane`].
pub fn forwards_unknowns(provenance: RequestProvenance, egress: EgressKind) -> bool {
    IngressDialect::of_provenance(provenance).is_none_or(|ingress| egress_speaks(ingress, egress))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{EgressKind, IngressDialect, Lane, LaneClass, forwards_unknowns};
    use crate::RequestProvenance;
    use crate::safe_token::is_safe_token;

    const FIDELITY_PAIRS: [(IngressDialect, EgressKind); 7] = [
        (IngressDialect::Anthropic, EgressKind::AnthropicApi),
        (IngressDialect::Anthropic, EgressKind::AnthropicApiMantle),
        (IngressDialect::Anthropic, EgressKind::BedrockInvoke),
        (IngressDialect::Chat, EgressKind::OpenaiCompat),
        (IngressDialect::Chat, EgressKind::OpenaiCompatMantle),
        (IngressDialect::Responses, EgressKind::OpenaiResponses),
        (IngressDialect::Responses, EgressKind::OpenaiResponsesMantle),
    ];

    #[test]
    fn fidelity_exactly_for_same_dialect_pairs() {
        for ingress in IngressDialect::ALL {
            for egress in EgressKind::ALL {
                let lane = Lane::of(ingress, egress);
                let expected = if FIDELITY_PAIRS.contains(&(ingress, egress)) {
                    LaneClass::Fidelity
                } else {
                    LaneClass::Translation
                };

                assert_eq!(lane.class(), expected, "lane {}", lane.label());
            }
        }
    }

    const PROVENANCES: [RequestProvenance; 4] = [
        RequestProvenance::Library,
        RequestProvenance::AnthropicIngress,
        RequestProvenance::OpenaiIngress,
        RequestProvenance::ResponsesIngress,
    ];

    #[test]
    fn forwards_unknowns_agrees_with_the_lane_class_and_always_for_a_library_caller() {
        for provenance in PROVENANCES {
            for egress in EgressKind::ALL {
                // Act
                let forwards = forwards_unknowns(provenance, egress);

                // Assert
                let expected = IngressDialect::of_provenance(provenance)
                    .is_none_or(|ingress| Lane::of(ingress, egress).class() == LaneClass::Fidelity);
                assert_eq!(forwards, expected, "{provenance:?} -> {}", egress.label());
            }
        }
    }

    #[test]
    fn lane_indices_are_dense_and_unique_over_every_pair() {
        let mut seen = [false; Lane::COUNT];
        for ingress in IngressDialect::ALL {
            for egress in EgressKind::ALL {
                let index = Lane::of(ingress, egress).index();

                assert!(index < Lane::COUNT, "index {index} out of range");
                if let Some(slot) = seen.get_mut(index) {
                    assert!(!*slot, "duplicate index {index}");
                    *slot = true;
                }
            }
        }
        assert!(seen.iter().all(|s| *s), "every index in 0..COUNT is used");
    }

    #[test]
    fn every_lane_label_is_unique_safe_and_names_both_ends() {
        let mut seen = HashSet::new();
        for ingress in IngressDialect::ALL {
            for egress in EgressKind::ALL {
                let label = Lane::of(ingress, egress).label();

                assert!(is_safe_token(label), "{label:?}");
                assert_eq!(label, format!("{}->{}", ingress.label(), egress.label()));
                assert!(seen.insert(label), "duplicate lane label {label}");
            }
        }
        assert_eq!(
            seen.len(),
            IngressDialect::ALL.len() * EgressKind::ALL.len()
        );
    }
}
