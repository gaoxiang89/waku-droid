//! Markdown images read from the host that owns the document.
//!
//! This is VS Code's Markdown preview over Remote-SSH in native form: the
//! renderer resolves an image to a path on the document's host, and the bytes
//! travel over that host's existing daemon connection instead of being read
//! from this computer's disk. Reads run as a GPUI asset load on the background
//! executor, so a frame only consults the asset cache and a finished load
//! redraws the view that asked for it.

use std::hash::{Hash, Hasher};

use base64::Engine as _;
use gpui::{ImageCacheError, RenderImage};

use super::*;
use crate::md::image_path::{self, ImagePath};

/// Larger images show their URL instead; a screenshot is a few megabytes.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// One image on one host. The host generation is part of the identity, so a
/// reconnect reads again instead of reusing what the old connection returned.
#[derive(Clone)]
pub(super) struct HostImage {
    host: HostKey,
    generation: u64,
    path: ImagePath,
    workspace: waku_client::WorkspaceClient,
}

impl PartialEq for HostImage {
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host && self.generation == other.generation && self.path == other.path
    }
}

impl Eq for HostImage {}

impl Hash for HostImage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.host.hash(state);
        self.generation.hash(state);
        self.path.hash(state);
    }
}

/// Asset loader that reads and decodes one host image.
pub(super) enum HostImageAsset {}

impl gpui::Asset for HostImageAsset {
    type Source = HostImage;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            let HostImage {
                path, workspace, ..
            } = source;
            let (bytes, name) = match read_host_file(&workspace, &path.path) {
                Ok(bytes) => (bytes, path.path),
                Err(error) => match path.fallback {
                    Some(fallback) => (read_host_file(&workspace, &fallback)?, fallback),
                    None => return Err(error.into()),
                },
            };
            let format = super::image_preview::image_format_for_name(&name)
                .or_else(|| sniff_format(&bytes))
                .ok_or_else(|| anyhow::anyhow!("{name} is not an image"))?;
            Ok(gpui::Image::from_bytes(format, bytes).to_image_data(renderer)?)
        }
    }
}

impl Waku {
    /// The loader that reads Markdown images from `host` through its daemon,
    /// or `None` when the host is gone.
    pub(super) fn markdown_image_loader(&self, host: &HostKey) -> Option<md::render::ImageLoader> {
        let host = self.hosts.get(host)?;
        let key = host.key.clone();
        let generation = host.generation;
        let workspace = waku_client::WorkspaceClient::for_daemon(host.daemon.clone());
        Some(Rc::new(move |path: &ImagePath| {
            let image = HostImage {
                host: key.clone(),
                generation,
                path: path.clone(),
                workspace: workspace.clone(),
            };
            gpui::ImageSource::Custom(Arc::new(move |window, cx| {
                window.use_asset::<HostImageAsset>(&image, cx)
            }))
        }))
    }
}

/// Read a whole file from the daemon in bounded chunks. Blocking; runs on the
/// background executor.
fn read_host_file(workspace: &waku_client::WorkspaceClient, path: &str) -> anyhow::Result<Vec<u8>> {
    let (root, name) =
        image_path::split_parent(path).ok_or_else(|| anyhow::anyhow!("not a file path: {path}"))?;
    let mut bytes = Vec::new();
    loop {
        let response = workspace.request(waku_client::WorkspaceOperation::ReadFileChunk {
            root: PathBuf::from(root),
            relative_path: PathBuf::from(name),
            offset: bytes.len() as u64,
            length: MAX_IMAGE_BYTES,
        })?;
        let waku_client::WorkspaceResult::FileChunk {
            bytes: chunk,
            total_bytes,
        } = response
        else {
            anyhow::bail!("the daemon returned an invalid file response");
        };
        if total_bytes > MAX_IMAGE_BYTES {
            anyhow::bail!("{path} is larger than {} MB", MAX_IMAGE_BYTES / 1024 / 1024);
        }
        let chunk = base64::engine::general_purpose::STANDARD.decode(chunk.trim())?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 >= total_bytes {
            break;
        }
    }
    Ok(bytes)
}

/// The format of an image whose name does not say, from its magic bytes.
fn sniff_format(bytes: &[u8]) -> Option<gpui::ImageFormat> {
    if bytes.starts_with(b"\x89PNG") {
        Some(gpui::ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(gpui::ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(gpui::ImageFormat::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(gpui::ImageFormat::Webp)
    } else if bytes.starts_with(b"BM") {
        Some(gpui::ImageFormat::Bmp)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(sniff_format(b"\x89PNG\r\n"), Some(gpui::ImageFormat::Png));
        assert_eq!(
            sniff_format(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(gpui::ImageFormat::Jpeg)
        );
        assert_eq!(
            sniff_format(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(gpui::ImageFormat::Webp)
        );
        assert_eq!(sniff_format(b"plain text"), None);
    }
}
