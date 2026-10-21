//! Mermaid fences drawn as diagrams through ondine's Rust compiler.
//!
//! Compiling and rasterizing both run inside a GPUI [`Asset`] load, so they
//! happen on the background executor and the result is cached by source and
//! theme. A render frame only asks the asset cache, and a miss shows the
//! fence's source until the image lands.

use std::sync::Arc;

use gpui::{App, Hsla, ImageCacheError, ImageFormat, RenderImage, Rgba, SharedString};

mod resolve;

use resolve::{DiagramTheme, Rgb, flatten_theme};

use super::render::Palette;

/// Mono stack for diagram labels. ondine lays text out on a uniform advance;
/// the SVG renderer's font database holds system faces rather than the fonts
/// Waku registers with its text system, so name the platforms' own monos too.
const FONT_FAMILY: &str =
    "'JetBrains Mono', 'Cascadia Mono', Consolas, Menlo, 'SF Mono', 'DejaVu Sans Mono', monospace";

/// Whether a fence's info string names a Mermaid diagram.
pub fn is_mermaid(language: Option<&str>) -> bool {
    language.is_some_and(|language| language.eq_ignore_ascii_case("mermaid"))
}

/// One diagram to draw: its source, label size, and the colors it is drawn in.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DiagramSource {
    source: SharedString,
    font_size: u32,
    theme: DiagramTheme,
}

impl DiagramSource {
    pub fn new(source: SharedString, font_size: f32, palette: &Palette) -> Self {
        let background = opaque(palette.inset, palette.is_dark());
        let foreground = opaque(palette.text, palette.is_dark());
        Self {
            source,
            font_size: font_size.round() as u32,
            theme: DiagramTheme {
                background,
                foreground,
                muted: opaque(palette.secondary, palette.is_dark()),
                line: foreground.mix(background, 0.5),
                surface: foreground.mix(background, 0.06),
                surface_alt: foreground.mix(background, 0.14),
                border: foreground.mix(background, 0.32),
                accent: opaque(palette.accent, palette.is_dark()),
                font_family: FONT_FAMILY,
            },
        }
    }
}

/// `color` composited onto the window's base so SVG paint stays opaque.
fn opaque(color: Hsla, is_dark: bool) -> Rgb {
    let Rgba { r, g, b, a } = color.into();
    let base = if is_dark { 0.0 } else { 1.0 };
    let channel = |value: f32| ((value * a + base * (1.0 - a)) * 255.0).round() as u8;
    Rgb(channel(r), channel(g), channel(b))
}

/// The SVG for `diagram`, themed and ready for usvg.
fn compile(diagram: &DiagramSource) -> anyhow::Result<String> {
    let options = format!(r#"{{"fontSize":{}}}"#, diagram.font_size);
    let rendered = ondine::render(&diagram.source, &options, f64::NAN)
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    Ok(flatten_theme(&rendered.svg, &diagram.theme))
}

/// Asset loader that compiles and rasterizes one diagram.
pub enum MermaidDiagram {}

impl gpui::Asset for MermaidDiagram {
    type Source = DiagramSource;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            let svg = compile(&source).map_err(|error| ImageCacheError::Other(Arc::new(error)))?;
            gpui::Image::from_bytes(ImageFormat::Svg, svg.into_bytes())
                .to_image_data(renderer)
                .map_err(|error| ImageCacheError::Other(Arc::new(error)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_flowchart_without_unresolved_theme() {
        let palette = Palette::from_theme(&crate::theme::Theme::dark());
        let diagram = DiagramSource::new("flowchart LR\n  A --> B\n".into(), 14.0, &palette);
        let svg = compile(&diagram).expect("flowchart should compile");
        assert!(svg.starts_with("<svg"));
        assert!(!svg.contains("var("), "unresolved theme variable: {svg}");
    }

    #[test]
    fn rejects_unknown_diagrams() {
        let palette = Palette::from_theme(&crate::theme::Theme::dark());
        let diagram = DiagramSource::new("not a diagram".into(), 14.0, &palette);
        assert!(compile(&diagram).is_err());
    }
}
