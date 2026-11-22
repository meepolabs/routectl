// The style contract: every color token has a light value, no rule paints a
// raw color, and the motion and series tokens hold the limits the page's
// direction sets. `include!`d into a child module of `page::tests`, so the
// helpers there (`style`, `strip_css_comments`, `compact`) are in scope; do
// not add `use` lines here.

use std::collections::BTreeMap;

/// One CSS declaration and the block preludes enclosing it, outermost first
/// (`["@media (prefers-color-scheme: light)", ":root"]`).
#[derive(Debug)]
struct Decl {
    context: Vec<String>,
    prop: String,
    value: String,
}

/// Every declaration in `css` (comments stripped). A segment ending in `{` is
/// a block prelude; one ending in `;` or `}` inside a block is a declaration.
/// Quoted strings are skipped so `content: ";"` stays one value.
fn css_decls(css: &str) -> Vec<Decl> {
    let src = strip_css_comments(css);
    let mut decls = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut seg = String::new();
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                seg.push(c);
                for q in chars.by_ref() {
                    seg.push(q);
                    if q == c {
                        break;
                    }
                }
            }
            '{' => {
                stack.push(seg.trim().to_string());
                seg.clear();
            }
            ';' | '}' => {
                if let Some((prop, value)) = seg.split_once(':')
                    && !stack.is_empty()
                {
                    decls.push(Decl {
                        context: stack.clone(),
                        prop: prop.trim().to_string(),
                        value: value.trim().to_string(),
                    });
                }
                seg.clear();
                if c == '}' {
                    stack.pop();
                }
            }
            _ => seg.push(c),
        }
    }
    decls
}

const COLOR_FUNCTIONS: &[&str] = &[
    "rgb",
    "rgba",
    "hsl",
    "hsla",
    "hwb",
    "lab",
    "lch",
    "oklab",
    "oklch",
    "color",
    "color-mix",
];

/// The first raw color literal in a declaration value: a hex color or a CSS
/// color function. `var(...)`, `transparent` and `currentColor` are not raw.
fn raw_color_literal(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for (i, _) in lower.match_indices('#') {
        let digits = bytes[i + 1..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        let ends_word = bytes
            .get(i + 1 + digits)
            .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_'));
        if matches!(digits, 3 | 4 | 6 | 8) && ends_word {
            return Some(lower[i..i + 1 + digits].to_string());
        }
    }
    for (i, _) in lower.match_indices('(') {
        let name_start = lower[..i]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .map_or(0, |p| p + 1);
        let name = &lower[name_start..i];
        if COLOR_FUNCTIONS.contains(&name) {
            return Some(format!("{name}("));
        }
    }
    None
}

fn is_dark_root(d: &Decl) -> bool {
    d.context.len() == 1 && d.context[0] == ":root"
}

fn is_light_root(d: &Decl) -> bool {
    d.context.len() == 2
        && d.context[0].starts_with("@media")
        && compact(&d.context[0]).contains("prefers-color-scheme:light")
        && d.context[1] == ":root"
}

/// Custom properties the top-level `:root` gives a raw color literal that
/// the light `:root` never re-declares. A `var()` alias carries no literal,
/// so it follows its target into the light theme and is never reported.
fn color_tokens_without_light_value(css: &str) -> Vec<String> {
    let decls = css_decls(css);
    let light: Vec<&str> = decls
        .iter()
        .filter(|d| is_light_root(d))
        .map(|d| d.prop.as_str())
        .collect();
    decls
        .iter()
        .filter(|d| is_dark_root(d) && d.prop.starts_with("--"))
        .filter(|d| raw_color_literal(&d.value).is_some())
        .filter(|d| !light.contains(&d.prop.as_str()))
        .map(|d| d.prop.clone())
        .collect()
}

/// `<prop>: <value>` for every declaration that is not a custom property and
/// still carries a raw color literal.
fn raw_colors_outside_tokens(css: &str) -> Vec<String> {
    css_decls(css)
        .into_iter()
        .filter(|d| !d.prop.starts_with("--"))
        .filter(|d| raw_color_literal(&d.value).is_some())
        .map(|d| format!("{}: {}", d.prop, d.value))
        .collect()
}

/// The custom properties as one theme resolves them: the top-level `:root`,
/// overlaid with the light `:root` when `light`.
fn theme_tokens(css: &str, light: bool) -> BTreeMap<String, String> {
    let mut tokens = BTreeMap::new();
    for d in css_decls(css) {
        if d.prop.starts_with("--") && (is_dark_root(&d) || (light && is_light_root(&d))) {
            tokens.insert(d.prop, d.value);
        }
    }
    tokens
}

/// A token's value with `var(--x)` aliases followed to the literal.
fn resolve<'a>(tokens: &'a BTreeMap<String, String>, name: &str) -> &'a str {
    let mut value = tokens
        .get(name)
        .unwrap_or_else(|| panic!("token {name} is not declared"));
    for _ in 0..8 {
        let Some(target) = value.strip_prefix("var(").and_then(|v| v.strip_suffix(')')) else {
            return value;
        };
        value = tokens
            .get(target.trim())
            .unwrap_or_else(|| panic!("{name} aliases undeclared {target}"));
    }
    panic!("{name} aliases too deep")
}

/// The numbers inside `<fn>(...)`, separators and `/` ignored.
fn fn_args(value: &str, func: &str) -> Vec<f64> {
    let inner = value
        .strip_prefix(&format!("{func}("))
        .and_then(|v| v.strip_suffix(')'))
        .unwrap_or_else(|| panic!("`{value}` is not a {func}() color"));
    inner
        .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<f64>()
                .unwrap_or_else(|_| panic!("`{s}` in `{value}`"))
        })
        .collect()
}

/// Gamma-encoded sRGB in 0..=1 (clamped) plus alpha, for the forms the
/// tokens use: 6-digit hex, `rgba(r, g, b, a)` and `oklch(L C h)`.
fn srgba(value: &str) -> [f64; 4] {
    if let Some(hex) = value.strip_prefix('#') {
        let channel = |i: usize| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap()) / 255.0;
        return [channel(0), channel(2), channel(4), 1.0];
    }
    if value.starts_with("rgba(") {
        let a = fn_args(value, "rgba");
        return [a[0] / 255.0, a[1] / 255.0, a[2] / 255.0, a[3]];
    }
    let [l, c, h] = fn_args(value, "oklch")[..] else {
        panic!("`{value}` is not oklch(L C h)");
    };
    let lab = [l, c * h.to_radians().cos(), c * h.to_radians().sin()];
    let lms = [
        [1.0, 0.396_337_777_4, 0.215_803_757_3],
        [1.0, -0.105_561_345_8, -0.063_854_172_8],
        [1.0, -0.089_484_177_5, -1.291_485_548_0],
    ]
    .map(|row| dot(row, lab).powi(3));
    let linear = [
        [4.076_741_662_1, -3.307_711_591_3, 0.230_969_929_2],
        [-1.268_438_004_6, 2.609_757_401_1, -0.341_319_396_5],
        [-0.004_196_086_3, -0.703_418_614_7, 1.707_614_701_0],
    ]
    .map(|row| dot(row, lms));
    let encode = |x: f64| {
        let x = x.clamp(0.0, 1.0);
        if x <= 0.003_130_8 {
            12.92 * x
        } else {
            x.powf(1.0 / 2.4).mul_add(1.055, -0.055)
        }
    };
    [encode(linear[0]), encode(linear[1]), encode(linear[2]), 1.0]
}

/// `fg` alpha-composited over an opaque `bg`.
fn over(fg: [f64; 4], bg: [f64; 4]) -> [f64; 4] {
    let mix = |i: usize| fg[3].mul_add(fg[i], (1.0 - fg[3]) * bg[i]);
    [mix(0), mix(1), mix(2), 1.0]
}

/// WCAG 2.x relative luminance of an opaque sRGB color.
fn relative_luminance(rgb: [f64; 4]) -> f64 {
    let lin = |c: f64| {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    dot(
        [0.2126, 0.7152, 0.0722],
        [lin(rgb[0]), lin(rgb[1]), lin(rgb[2])],
    )
}

fn dot(row: [f64; 3], v: [f64; 3]) -> f64 {
    row[0].mul_add(v[0], row[1].mul_add(v[1], row[2] * v[2]))
}

fn contrast(a: [f64; 4], b: [f64; 4]) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn hue_of(tokens: &BTreeMap<String, String>, name: &str) -> f64 {
    fn_args(resolve(tokens, name), "oklch")[2]
}

fn hue_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

fn series_names() -> Vec<String> {
    (1..=6).map(|n| format!("--series-{n}")).collect()
}

/// The `(prop, value)` declarations of the first block whose innermost
/// prelude is `selector` inside `outer` (empty for top level).
fn block<'a>(decls: &'a [Decl], outer: &str, selector: &str) -> Vec<(&'a str, &'a str)> {
    decls
        .iter()
        .filter(|d| {
            d.context.last().is_some_and(|s| s == selector)
                && d.context[..d.context.len() - 1].join(" ") == outer
        })
        .map(|d| (d.prop.as_str(), d.value.as_str()))
        .collect()
}

fn ms(value: &str) -> f64 {
    value
        .strip_suffix("ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("`{value}` is not a millisecond duration"))
}

/// The browser's default font size the page's rem sizes are read against.
const DEFAULT_FONT_PX: f64 = 16.0;

/// The size `selector`'s last top-level `font-size` declaration computes to
/// in one theme at the default font, `var(--x)` followed to a `rem` or `px`
/// literal.
fn font_size_px(css: &str, selector: &str, light: bool) -> f64 {
    let decls = css_decls(css);
    let (_, value) = block(&decls, "", selector)
        .into_iter()
        .rfind(|(prop, _)| *prop == "font-size")
        .unwrap_or_else(|| panic!("{selector} declares no font-size"));
    let tokens = theme_tokens(css, light);
    let value = match value.strip_prefix("var(").and_then(|v| v.strip_suffix(')')) {
        Some(name) => resolve(&tokens, name.trim()),
        None => value,
    };
    let number = |unit: &str| value.strip_suffix(unit).and_then(|v| v.parse::<f64>().ok());
    number("rem")
        .map(|rem| rem * DEFAULT_FONT_PX)
        .or_else(|| number("px"))
        .unwrap_or_else(|| panic!("{selector}: `{value}` is not a rem or px size"))
}

#[test]
fn every_color_token_has_a_light_value() {
    let css = style();
    let light_declared: Vec<String> = css_decls(&css)
        .into_iter()
        .filter(is_light_root)
        .map(|d| d.prop)
        .collect();
    for name in ["--bg", "--series-1", "--backdrop"] {
        assert!(
            light_declared.iter().any(|p| p == name),
            "positive control: the scan finds {name} in the light block"
        );
    }

    assert_eq!(color_tokens_without_light_value(&css), Vec::<String>::new());
}

#[test]
fn a_color_token_defined_only_for_dark_is_reported() {
    let css = ":root { --a: #101010; --b: oklch(0.7 0.1 250); --c: var(--a); --d: 4px; }\n\
               @media (prefers-color-scheme: light) { :root { --a: #fefefe; } }";

    assert_eq!(
        color_tokens_without_light_value(css),
        vec!["--b".to_string()]
    );
}

#[test]
fn no_rule_paints_a_raw_color() {
    let css = style();
    assert!(
        css_decls(&css)
            .iter()
            .any(|d| d.prop == "background" && d.value == "var(--accent)"),
        "positive control: the scan reads rule declarations"
    );

    assert_eq!(raw_colors_outside_tokens(&css), Vec::<String>::new());
}

#[test]
fn a_raw_color_in_a_rule_is_reported() {
    let cases = [
        ("hex", ".a { color: #fff; }", "color: #fff"),
        (
            "rgba inside a shorthand",
            ".a { box-shadow: 0 0 0 3px rgba(0, 0, 0, 0.2); }",
            "box-shadow: 0 0 0 3px rgba(0, 0, 0, 0.2)",
        ),
        (
            "keyframe frame",
            "@keyframes k { from { background-color: oklch(0.5 0.1 20); } }",
            "background-color: oklch(0.5 0.1 20)",
        ),
        (
            "hsl",
            "@media (x) { .a { fill: hsl(0 0% 0%) } }",
            "fill: hsl(0 0% 0%)",
        ),
    ];
    for (label, rule, expected) in cases {
        let css = format!(
            ":root {{ --t: #000; }} #modal-host {{ color: var(--t); background: transparent; }} {rule}"
        );

        assert_eq!(
            raw_colors_outside_tokens(&css),
            vec![expected.to_string()],
            "{label}"
        );
    }
}

#[test]
fn the_change_highlight_lasts_400ms_and_fades_only_its_background() {
    let css = style();
    let decls = css_decls(&css);
    for light in [false, true] {
        let dur = ms(resolve(&theme_tokens(&css, light), "--dur-change"));
        assert_eq!(dur, 400.0, "--dur-change (light: {light})");
    }
    let changed = block(&decls, "", ".changed");
    assert_eq!(
        changed,
        vec![("animation", "changefade var(--dur-change) var(--ease)")]
    );
    let frames: Vec<&str> = decls
        .iter()
        .filter(|d| {
            d.context
                .first()
                .is_some_and(|c| c == "@keyframes changefade")
        })
        .map(|d| d.prop.as_str())
        .collect();
    assert!(!frames.is_empty(), "the changefade keyframes exist");
    for prop in frames {
        assert!(
            prop == "background-color" || prop == "opacity",
            "changefade animates `{prop}`"
        );
    }
}

/// The Now tile sub-labels (the tile names, and the keys of a split tile
/// such as IN / OUT and READ / WRITE) read at the page's small text size,
/// never below 11 px at the default font, in both themes.
#[test]
fn the_tile_sub_labels_are_at_least_eleven_px() {
    let css = style();
    for selector in [".kpi-label", ".kpi-key"] {
        for light in [false, true] {
            let px = font_size_px(&css, selector, light);

            assert!(px >= 11.0, "{selector} is {px}px (light: {light})");
        }
    }
}

#[test]
fn the_font_size_reader_follows_tokens_and_units() {
    let css = ":root { --small: 0.6rem; --xs: 0.72rem; }\n\
               @media (prefers-color-scheme: light) { :root { --xs: 13px; } }\n\
               .rem { font-size: 0.58rem; } .tok { font-size: var(--xs); }\n\
               .px { color: var(--a); font-size: 10px; } .px { font-size: 12px; }";
    let cases = [
        ("a rem literal", ".rem", false, 9.28),
        ("a token, dark", ".tok", false, 11.52),
        ("a token the light theme overrides", ".tok", true, 13.0),
        ("the later of two declarations", ".px", false, 12.0),
    ];
    for (label, selector, light, expected) in cases {
        let px = font_size_px(css, selector, light);

        assert!((px - expected).abs() < 1e-9, "{label}: {px}");
    }
}

#[test]
fn reduced_motion_turns_the_change_highlight_off() {
    let decls = css_decls(&style());
    let reduced = "@media (prefers-reduced-motion: reduce)";
    let selectors: Vec<&str> = decls
        .iter()
        .filter(|d| d.context.len() == 2 && d.context[0] == reduced && d.prop == "animation")
        .filter(|d| d.value == "none")
        .map(|d| d.context[1].as_str())
        .collect();

    assert!(
        selectors
            .iter()
            .any(|s| s.split(',').any(|sel| sel.trim() == ".changed")),
        "the reduced-motion block sets `.changed` to `animation: none`; it covers {selectors:?}"
    );
}

#[test]
fn series_colors_are_distinct_and_keep_clear_of_the_state_hues() {
    let css = style();
    for light in [false, true] {
        let tokens = theme_tokens(&css, light);
        let states: Vec<f64> = ["--ok", "--warn", "--neg"]
            .iter()
            .map(|s| hue_of(&tokens, s))
            .collect();
        let values: Vec<&str> = series_names().iter().map(|n| resolve(&tokens, n)).collect();
        for (i, name) in series_names().iter().enumerate() {
            let hue = hue_of(&tokens, name);
            for state in &states {
                assert!(
                    hue_distance(hue, *state) >= 40.0,
                    "{name} hue {hue} within 40 degrees of a state hue {state} (light: {light})"
                );
            }
            assert!(
                !values[..i].contains(&values[i]),
                "{name} repeats an earlier series (light: {light})"
            );
        }
    }
}

#[test]
fn series_colors_keep_three_to_one_against_the_surface() {
    let css = style();
    for light in [false, true] {
        let tokens = theme_tokens(&css, light);
        let surface = srgba(resolve(&tokens, "--surface"));
        for name in series_names() {
            let ratio = contrast(srgba(resolve(&tokens, &name)), surface);
            assert!(
                ratio >= 3.0,
                "{name} on --surface is {ratio:.2}:1 (light: {light})"
            );
        }
    }
}

#[test]
fn the_active_group_by_button_text_keeps_four_and_a_half_to_one() {
    let css = style();
    let decls = css_decls(&css);
    assert_eq!(
        block(&decls, "", ".gbbtn--on"),
        vec![
            ("background", "var(--accent-soft)"),
            ("color", "var(--accent-ink)")
        ]
    );
    for light in [false, true] {
        let tokens = theme_tokens(&css, light);
        for page in ["--surface", "--bg"] {
            let base = srgba(resolve(&tokens, page));
            let fill = over(srgba(resolve(&tokens, "--accent-soft")), base);
            let ratio = contrast(srgba(resolve(&tokens, "--accent-ink")), fill);
            assert!(ratio >= 4.5, "on {page}: {ratio:.2}:1 (light: {light})");
        }
    }
}

/// The pressed range button is marked by more than its fill: accent text
/// and an underline, as the pressed Group by button is, so a hovered button
/// (a fill alone) never reads as a second selection. The text keeps 4.5:1 on
/// its fill in both themes.
///
/// Mutation check: set the pressed rule back to `background:
/// var(--surface-2); color: var(--fg);` -> red.
#[test]
fn the_pressed_range_button_is_marked_by_text_and_underline() {
    let css = style();
    let decls = css_decls(&css);
    let pressed = block(&decls, "", ".windowsel button[aria-pressed=\"true\"]");
    let hover = block(&decls, "", ".windowsel button:hover");
    let value = |rules: &[(&str, &str)], prop: &str| {
        rules
            .iter()
            .rfind(|(p, _)| *p == prop)
            .map(|(_, v)| (*v).to_owned())
    };
    assert_eq!(
        value(&pressed, "color").as_deref(),
        Some("var(--accent-ink)")
    );
    assert!(
        value(&pressed, "box-shadow").is_some_and(|v| v.contains("var(--accent)")),
        "{pressed:?}"
    );
    assert_ne!(value(&pressed, "color"), value(&hover, "color"));
    assert!(value(&hover, "box-shadow").is_none(), "{hover:?}");
    for light in [false, true] {
        let tokens = theme_tokens(&css, light);
        let base = srgba(resolve(&tokens, "--surface"));
        let fill = over(srgba(resolve(&tokens, "--accent-soft")), base);
        let ratio = contrast(srgba(resolve(&tokens, "--accent-ink")), fill);
        assert!(ratio >= 4.5, "{ratio:.2}:1 (light: {light})");
    }
}

/// Faint text on a selected provider card (the accent tint over the page,
/// a card or the raised surface) keeps 4.5:1 in both themes.
///
/// Mutation check: set the light `--faint` back to `#646e79` -> red (4.33:1
/// on the tint over --bg).
#[test]
fn faint_text_on_a_selected_card_keeps_four_and_a_half_to_one() {
    let css = style();
    let decls = css_decls(&css);
    assert!(
        block(&decls, "", ".provcard--on").contains(&("background", "var(--accent-soft)")),
        "the selected card is tinted with --accent-soft"
    );
    for light in [false, true] {
        let tokens = theme_tokens(&css, light);
        let faint = srgba(resolve(&tokens, "--faint"));
        let tint = srgba(resolve(&tokens, "--accent-soft"));
        for bg in TEXT_BACKGROUNDS {
            let fill = over(tint, srgba(resolve(&tokens, bg)));
            let ratio = contrast(faint, fill);
            assert!(
                ratio >= 4.5,
                "on the tint over {bg}: {ratio:.2}:1 (light: {light})"
            );
        }
    }
}

#[test]
fn the_contrast_formula_matches_the_wcag_reference_points() {
    let black = srgba("#000000");
    let white = srgba("#ffffff");

    assert!((contrast(black, white) - 21.0).abs() < 1e-9);
    assert!((contrast(srgba("#767676"), white) - 4.54).abs() < 0.01);
    assert!((relative_luminance(srgba("oklch(1 0 0)")) - 1.0).abs() < 1e-3);
}

/// The classes that mark a reading or a verdict as old: the stale meters
/// and traffic figures, the last known verdict kept through an outage and a
/// tab badge whose source is not current. Text inside them is dimmed by
/// color, never by opacity; an opacity is allowed only on a mark (a meter
/// fill, the verdict dot).
const STALE_READING_CLASSES: &[&str] = &[
    ".glance-meter--stale",
    ".acct-win--stale",
    ".acct-traffic--stale",
    ".verdict--stale",
    ".tab-badge--dimmed",
];

/// The backgrounds a reading's text is drawn on.
const TEXT_BACKGROUNDS: &[&str] = &["--bg", "--surface", "--surface-2"];

/// A tab badge's tone and the pill background it draws its text on. The
/// pill is translucent and the tab bar under it is `--surface`.
const BADGE_PILLS: &[(&str, &str)] = &[
    (".tab-badge--warn", "--pill-warn-bg"),
    (".tab-badge--error", "--pill-err-bg"),
    (".tab-badge--muted", "--pill-neutral-bg"),
];

/// Every `(selector, prop, value)` declared at top level whose selector names
/// `class`, one entry per selector of a selector list.
fn decls_naming(css: &str, class: &str) -> Vec<(String, String, String)> {
    css_decls(css)
        .into_iter()
        .filter(|d| d.context.len() == 1)
        .flat_map(|d| {
            d.context[0]
                .split(',')
                .map(str::trim)
                .filter(|sel| {
                    sel.split(|c: char| c.is_whitespace() || c == '>')
                        .any(|part| part == class || part.starts_with(&format!("{class}.")))
                })
                .map(|sel| (sel.to_string(), d.prop.clone(), d.value.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The opaque backgrounds the text `sel` colors is drawn on in one theme:
/// its pill over the tab bar for a badge of a known tone, else every reading
/// background.
fn text_backgrounds(sel: &str, tokens: &BTreeMap<String, String>) -> Vec<(String, [f64; 4])> {
    if let Some((_, pill)) = BADGE_PILLS.iter().find(|(tone, _)| sel.contains(tone)) {
        let bar = srgba(resolve(tokens, "--surface"));
        return vec![(
            format!("{pill} over --surface"),
            over(srgba(resolve(tokens, pill)), bar),
        )];
    }
    TEXT_BACKGROUNDS
        .iter()
        .map(|bg| ((*bg).to_string(), srgba(resolve(tokens, bg))))
        .collect()
}

/// What breaks the stale-reading text rule in `css`: an opacity on anything
/// but a mark, or a text color under 4.5:1 on its background in either
/// theme. Empty when the rule holds.
fn stale_text_faults(css: &str) -> Vec<String> {
    let mut faults = Vec::new();
    for class in STALE_READING_CLASSES {
        for (sel, prop, value) in decls_naming(css, class) {
            if prop == "opacity" && !sel.ends_with("-fill") && !sel.ends_with("-dot") {
                faults.push(format!("{sel} dims text with opacity {value}"));
            }
            if prop != "color" {
                continue;
            }
            let token = value
                .strip_prefix("var(")
                .and_then(|v| v.strip_suffix(')'))
                .unwrap_or_else(|| panic!("{sel}: color `{value}` is not a token"));
            for light in [false, true] {
                let tokens = theme_tokens(css, light);
                let text = srgba(resolve(&tokens, token));
                for (bg, behind) in text_backgrounds(&sel, &tokens) {
                    let ratio = contrast(text, behind);
                    if ratio < 4.5 {
                        faults.push(format!("{sel} on {bg} is {ratio:.2}:1 (light: {light})"));
                    }
                }
            }
        }
    }
    faults
}

/// Old text keeps 4.5:1 in both themes: no rule dims a stale reading, the
/// verdict kept through an outage or a dimmed tab badge with opacity, and
/// every color a stale rule gives text passes on its background (a badge on
/// its own pill). Positive control: each stale class recolors some text,
/// and both badge tones that carry a number are recolored when dimmed.
///
/// Mutation checks: restore `.acct-win--stale { opacity: 0.6; }` -> red;
/// restore `.verdict--stale .verdict-text { opacity: 0.55; }` -> red;
/// restore `.tab-badge--dimmed { opacity: 0.55; }` -> red.
#[test]
fn stale_reading_text_keeps_four_and_a_half_to_one_in_both_themes() {
    let css = style();
    for class in STALE_READING_CLASSES {
        assert!(
            decls_naming(&css, class)
                .iter()
                .any(|(_, prop, _)| prop == "color"),
            "{class} recolors no text; the scan is not seeing it"
        );
    }
    let badge_colors = decls_naming(&css, ".tab-badge--dimmed");
    for tone in [".tab-badge--warn", ".tab-badge--error"] {
        assert!(
            badge_colors
                .iter()
                .any(|(sel, prop, _)| prop == "color" && sel.contains(tone)),
            "a dimmed {tone} badge keeps its full-strength text"
        );
    }

    assert_eq!(stale_text_faults(&css), Vec::<String>::new());
}

#[test]
fn the_stale_text_scan_reports_opacity_and_a_low_contrast_color() {
    let css = style();
    let dimmed = format!("{css}\n.acct-win--stale {{ opacity: 0.6; }}\n");
    let faded_verdict = format!("{css}\n.verdict--stale .verdict-text {{ opacity: 0.55; }}\n");
    let faded_badge = format!("{css}\n.tab-badge--dimmed {{ opacity: 0.55; }}\n");
    let faint_fill = format!("{css}\n.glance-meter--stale .glance-bar-fill {{ opacity: 0.5; }}\n");
    let low = format!("{css}\n.acct-traffic--stale .x {{ color: var(--border-strong); }}\n");
    let low_badge =
        format!("{css}\n.tab-badge--dimmed.tab-badge--warn {{ color: var(--border-strong); }}\n");

    assert_eq!(
        stale_text_faults(&dimmed),
        vec![".acct-win--stale dims text with opacity 0.6"]
    );
    assert_eq!(
        stale_text_faults(&faded_verdict),
        vec![".verdict--stale .verdict-text dims text with opacity 0.55"]
    );
    assert_eq!(
        stale_text_faults(&faded_badge),
        vec![".tab-badge--dimmed dims text with opacity 0.55"]
    );
    assert!(stale_text_faults(&faint_fill).is_empty());
    assert!(
        stale_text_faults(&low)
            .iter()
            .any(|f| f.starts_with(".acct-traffic--stale .x on --bg")),
        "{:?}",
        stale_text_faults(&low)
    );
    assert_eq!(
        stale_text_faults(&low_badge),
        vec![
            ".tab-badge--dimmed.tab-badge--warn on --pill-warn-bg over --surface is 1.16:1 \
             (light: false)",
            ".tab-badge--dimmed.tab-badge--warn on --pill-warn-bg over --surface is 1.35:1 \
             (light: true)"
        ]
    );
}

fn stale_mark_opacity(css: &str, light: bool) -> f64 {
    resolve(&theme_tokens(css, light), "--stale-mark-opacity")
        .parse()
        .expect("the stale mark opacity is a number")
}

/// A stale meter's fill is dimmed, and at that opacity every meter color
/// keeps 3:1 against its track and against the card, in both themes.
///
/// Mutation check: set the dark `--stale-mark-opacity` to 0.6 -> red
/// (`--meter-full` dimmed on `--border` is 2.80:1).
#[test]
fn a_dimmed_stale_meter_fill_keeps_three_to_one() {
    let css = style();
    for light in [false, true] {
        let tokens = theme_tokens(&css, light);
        let alpha = stale_mark_opacity(&css, light);
        assert!(alpha < 1.0, "the stale fill is dimmed");
        for meter in ["--meter", "--meter-warn", "--meter-full"] {
            let [r, g, b, _] = srgba(resolve(&tokens, meter));
            for behind in ["--border", "--surface"] {
                let base = srgba(resolve(&tokens, behind));
                let fill = over([r, g, b, alpha], base);
                let ratio = contrast(fill, base);
                assert!(
                    ratio >= 3.0,
                    "{meter} dimmed on {behind} is {ratio:.2}:1 (light: {light})"
                );
            }
        }
    }
}

/// The dark theme dims a stale meter fill further than the light theme,
/// whose 0.85 is the lowest that keeps 3:1 there.
///
/// Mutation checks: drop the light block's `--stale-mark-opacity` -> red
/// (light follows the dark 0.65); set the dark value back to 0.85 -> red.
#[test]
fn the_dark_theme_dims_a_stale_meter_fill_further_than_light() {
    let css = style();
    let (dark, light) = (
        stale_mark_opacity(&css, false),
        stale_mark_opacity(&css, true),
    );

    assert!((light - 0.85).abs() < 1e-9, "light is {light}");
    assert!(dark < light, "dark {dark} is not dimmer than light {light}");
}

/// The Now "see Accounts" link, the shared "see <view>" link (Doctor's
/// "see Models") and the Models nickname link keep 4.5:1 on the page and on
/// a card in both themes, and the shared link draws the accent focus ring.
///
/// Mutation checks: color `.glance-link` with `var(--accent)` -> red (light
/// on --bg is 4.43:1); drop the `.view-link` color -> red (no color); color
/// `.models-link` with `var(--accent)` -> red.
#[test]
fn the_see_view_links_keep_four_and_a_half_to_one() {
    let css = style();
    let decls = css_decls(&css);
    for sel in [".glance-link", ".view-link", ".models-link"] {
        let (_, value) = block(&decls, "", sel)
            .into_iter()
            .rfind(|(prop, _)| *prop == "color")
            .unwrap_or_else(|| panic!("{sel} declares a color"));
        let token = value
            .strip_prefix("var(")
            .and_then(|v| v.strip_suffix(')'))
            .expect("the link color is a token");
        for light in [false, true] {
            let tokens = theme_tokens(&css, light);
            for bg in TEXT_BACKGROUNDS {
                let ratio = contrast(srgba(resolve(&tokens, token)), srgba(resolve(&tokens, bg)));
                assert!(ratio >= 4.5, "{sel} on {bg}: {ratio:.2}:1 (light: {light})");
            }
        }
    }
    assert_eq!(
        block(&decls, "", ".view-link:focus-visible"),
        vec![
            ("outline", "2px solid var(--accent)"),
            ("outline-offset", "2px")
        ]
    );
}

/// A click selects a whole value only on a command the operator runs as
/// is: the Now and Accounts fixes, the Models cause fix and a command a
/// Doctor remediation quotes. A raw
/// token or an inline value stays selectable in part (Ctrl+A on it when
/// focused still takes all of it). Positive control: the fix commands are
/// found, so the scan sees `user-select` declarations.
///
/// Mutation check: add `user-select: all;` to `.attn-raw-code` -> red.
#[test]
fn only_fix_commands_select_whole_on_click() {
    let css = style();
    let mut whole: Vec<String> = css_decls(&css)
        .into_iter()
        .filter(|d| d.prop == "user-select" && d.value == "all")
        .flat_map(|d| {
            d.context.last().map_or_else(Vec::new, |sel| {
                sel.split(',').map(|s| s.trim().to_string()).collect()
            })
        })
        .collect();
    whole.sort();

    assert_eq!(
        whole,
        vec![
            ".acct-fix-code",
            ".attn-fix-code",
            ".docfix-code",
            ".models-cause-fix"
        ]
    );
}

/// A Doctor remediation reads as prose in the UI face with no box of its
/// own; only the commands it quotes are code, each in the chip the
/// attention fix uses (its surface, border and radius).
///
/// Mutation checks: set `.docfix-text` back to `var(--font-mono)` -> red;
/// drop the `.docfix-code` border -> red.
#[test]
fn a_remediation_is_prose_with_only_its_commands_as_code_chips() {
    let css = style();
    let decls = css_decls(&css);
    let value = |sel: &str, prop: &str| {
        block(&decls, "", sel)
            .into_iter()
            .rfind(|(p, _)| *p == prop)
            .map(|(_, v)| v.to_owned())
    };

    assert_eq!(
        value(".docfix-text", "font-family").as_deref(),
        Some("var(--font-ui)")
    );
    for prop in ["background", "border", "padding"] {
        assert_eq!(value(".docfix-text", prop), None, ".docfix-text {prop}");
    }
    assert_eq!(
        value(".docfix-code", "font-family").as_deref(),
        Some("var(--font-mono)")
    );
    for prop in ["background", "border", "border-radius"] {
        assert!(
            value(".attn-fix-code", prop).is_some(),
            "positive control: the attention chip declares {prop}"
        );
        assert_eq!(
            value(".docfix-code", prop),
            value(".attn-fix-code", prop),
            ".docfix-code {prop}"
        );
    }
}
