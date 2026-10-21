//! Flattens ondine's themed SVG into plain presentation values.
//!
//! ondine themes a diagram through CSS custom properties (`var(--_fg)`) and
//! `color-mix()`, which a browser resolves but resvg does not: an unresolved
//! paint silently draws black or nothing. Every themed value therefore gets
//! evaluated here, once, against concrete colors before the SVG is parsed.

/// An opaque sRGB color.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `self` blended toward `other`, keeping `amount` of `self`.
    pub fn mix(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        let channel =
            |a: u8, b: u8| (f32::from(a) * amount + f32::from(b) * (1.0 - amount)).round() as u8;
        Self(
            channel(self.0, other.0),
            channel(self.1, other.1),
            channel(self.2, other.2),
        )
    }

    fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    fn parse(value: &str) -> Option<Self> {
        let hex = value.trim().strip_prefix('#')?;
        let digit = |index: usize, width: usize| {
            let value = u8::from_str_radix(hex.get(index..index + width)?, 16).ok()?;
            Some(if width == 1 { value * 17 } else { value })
        };
        match hex.len() {
            3 => Some(Self(digit(0, 1)?, digit(1, 1)?, digit(2, 1)?)),
            6 => Some(Self(digit(0, 2)?, digit(2, 2)?, digit(4, 2)?)),
            _ => None,
        }
    }
}

/// Concrete values for ondine's private theme aliases.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DiagramTheme {
    pub background: Rgb,
    pub foreground: Rgb,
    pub muted: Rgb,
    pub line: Rgb,
    pub surface: Rgb,
    pub surface_alt: Rgb,
    pub border: Rgb,
    pub accent: Rgb,
    pub font_family: &'static str,
}

impl DiagramTheme {
    fn alias(&self, name: &str) -> Option<String> {
        let color = match name {
            "--_bg" => self.background,
            "--_fg" => self.foreground,
            "--_m" => self.muted,
            "--_l" => self.line,
            "--_s" => self.surface,
            "--_a" => self.surface_alt,
            "--_b" => self.border,
            "--_c" => self.accent,
            // The public names too, where ondine reads them directly.
            "--_sw" | "--ondine-stroke" => return Some("1.25px".to_owned()),
            "--_r" | "--ondine-radius" => return Some("4".to_owned()),
            "--_fm" | "--ondine-font" => return Some(self.font_family.to_owned()),
            "--_w" | "--_tw" | "--_hw" => return Some("600".to_owned()),
            _ => return None,
        };
        Some(color.hex())
    }
}

/// `svg` with every themed value resolved, ready for usvg.
pub fn flatten_theme(svg: &str, theme: &DiagramTheme) -> String {
    let out = resolve(svg, theme);
    // usvg reads a corner radius only as a presentation attribute.
    let out = out.replace(" style=\"rx:4\"", " rx=\"4\"");
    clean_root_style(&out)
}

/// `svg` with every `var()` and `color-mix()` replaced by its value.
fn resolve(svg: &str, theme: &DiagramTheme) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(at) = next_function(rest) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match call_end(tail) {
            Some(end) => {
                out.push_str(&evaluate(&tail[..end], theme));
                rest = &tail[end..];
            }
            None => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The root `style` declares the custom properties resolved above, which
/// usvg's style parser does not accept, plus a `font:<size>px monospace`
/// shorthand whose generic family would override `font-family`. Keep the
/// declarations usvg understands, with the shorthand reduced to its size.
fn clean_root_style(svg: &str) -> String {
    const OPEN: &str = "style=\"";
    let Some(start) = svg.find(OPEN).map(|at| at + OPEN.len()) else {
        return svg.to_owned();
    };
    let Some(end) = svg[start..].find('"').map(|end| start + end) else {
        return svg.to_owned();
    };
    let style = svg[start..end]
        .split(';')
        .map(str::trim)
        .filter(|declaration| !declaration.is_empty() && !declaration.starts_with("--"))
        .map(|declaration| match declaration.strip_prefix("font:") {
            Some(shorthand) => format!(
                "font-size:{}",
                shorthand.split_whitespace().next().unwrap_or_default()
            ),
            None => declaration.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(";");
    format!("{}{style}{}", &svg[..start], &svg[end..])
}

fn next_function(text: &str) -> Option<usize> {
    match (text.find("var("), text.find("color-mix(")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Byte length of the call at the start of `text`, through its closing paren.
fn call_end(text: &str) -> Option<usize> {
    let open = text.find('(')?;
    let mut depth = 0usize;
    for (index, byte) in text.bytes().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            b'"' | b'<' | b'>' => return None,
            _ => {}
        }
    }
    None
}

/// Splits `args` at top-level commas.
fn arguments(args: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, byte) in args.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&args[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&args[start..]);
    parts
}

fn evaluate(call: &str, theme: &DiagramTheme) -> String {
    let open = call.find('(').unwrap_or(0);
    let name = &call[..open];
    let inner = call.get(open + 1..call.len() - 1).unwrap_or("");
    match name {
        "var" => {
            let (property, fallback) = match inner.split_once(',') {
                Some((property, fallback)) => (property.trim(), Some(fallback)),
                None => (inner.trim(), None),
            };
            theme
                .alias(property)
                .or_else(|| fallback.map(|fallback| resolve(fallback.trim(), theme)))
                .unwrap_or_default()
        }
        _ => {
            // color-mix(in srgb, A p%, B)
            let parts = arguments(inner);
            let mixed = (|| {
                let (first, second) = (parts.get(1)?, parts.get(2)?);
                let (color, amount) = match first.trim().rsplit_once(' ') {
                    Some((color, amount)) if amount.ends_with('%') => (
                        color,
                        amount.trim_end_matches('%').parse::<f32>().ok()? / 100.0,
                    ),
                    _ => (first.trim(), 0.5),
                };
                let a = Rgb::parse(&resolve(color, theme))?;
                let b = Rgb::parse(&resolve(second.trim(), theme))?;
                Some(a.mix(b, amount).hex())
            })();
            mixed.unwrap_or_else(|| theme.foreground.hex())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> DiagramTheme {
        DiagramTheme {
            background: Rgb(0, 0, 0),
            foreground: Rgb(255, 255, 255),
            muted: Rgb(200, 200, 200),
            line: Rgb(150, 150, 150),
            surface: Rgb(20, 20, 20),
            surface_alt: Rgb(40, 40, 40),
            border: Rgb(100, 100, 100),
            accent: Rgb(0, 128, 255),
            font_family: "JetBrains Mono",
        }
    }

    #[test]
    fn resolves_aliases_fallbacks_and_mixes() {
        let svg = r##"<rect fill="var(--_s)" stroke="var(--ondine-border,var(--_b))"/><text fill="var(--ondine-series-2,color-mix(in srgb,var(--_fg) 50%,var(--_bg)))" font-weight="var(--_w)">a</text>"##;
        assert_eq!(
            flatten_theme(svg, &theme()),
            r##"<rect fill="#141414" stroke="#646464"/><text fill="#808080" font-weight="600">a</text>"##
        );
    }

    #[test]
    fn leaves_unthemed_text_alone() {
        let svg = r#"<text x="1">var is a keyword (sometimes)</text>"#;
        assert_eq!(flatten_theme(svg, &theme()), svg);
    }
}
