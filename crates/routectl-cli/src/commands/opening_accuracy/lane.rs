//! A served lane's report key: persisted provider kind, provider nickname,
//! model nickname and upstream model id, each bounded to printable ASCII so a
//! database value can never add a line or a terminal escape to the report.

use routectl_usage::StreamExtraRow;

/// The longest a single lane component is rendered.
pub(super) const MAX_COMPONENT_CHARS: usize = 48;

/// `kind:provider/model@upstream`, `-` for an absent component; `-` alone
/// when no lane served.
pub(super) fn lane_label(row: &StreamExtraRow) -> String {
    let parts = [&row.provider_kind, &row.provider, &row.model, &row.upstream];
    if parts.iter().all(|p| p.is_none()) {
        return "-".to_string();
    }
    let [kind, provider, model, upstream] = parts.map(|p| component(p.as_deref()));
    format!("{kind}:{provider}/{model}@{upstream}")
}

/// One component: printable ASCII kept, everything else `?`, truncated
/// with a trailing `...` past [`MAX_COMPONENT_CHARS`].
fn component(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "-".to_string();
    };
    let mut out: String = value
        .chars()
        .take(MAX_COMPONENT_CHARS)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    if value.chars().count() > MAX_COMPONENT_CHARS {
        out.push_str("...");
    }
    out
}
