//! Recognising a blob as a picture.
//!
//! Both the extension and the signature have to agree — see [`image_format`],
//! where the reasoning is.

/// An image format the blob view shows as a picture.
#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Svg,
}

impl ImageFormat {
    /// The MIME type given to the `Blob`. This, not the file name, is what the
    /// browser decodes by: the extension never leaves this module.
    pub(super) fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Svg => "image/svg+xml",
        }
    }

    /// The format `filename`'s extension claims (but not SVG)
    pub(super) fn from_extension(filename: &str) -> Option<Self> {
        let (_, ext) = filename.rsplit_once('.')?;
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(ImageFormat::Png),
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            "gif" => Some(ImageFormat::Gif),
            _ => None,
        }
    }

    /// Whether `data` opens with this format's signature. Only the leading
    /// bytes are examined: enough to tell the format apart from a file that
    /// merely claims the extension, which is all this is for.
    pub(super) fn matches_magic(self, data: &[u8]) -> bool {
        match self {
            ImageFormat::Png => data.starts_with(b"\x89PNG\r\n\x1a\n"),
            // SOI followed by the first marker's 0xFF. Every real JPEG variant
            // (JFIF, Exif, raw) shares this prefix and differs after it.
            ImageFormat::Jpeg => data.starts_with(b"\xff\xd8\xff"),
            ImageFormat::Gif => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
            // Unreachable, since `from_extension` never yields `Svg`: an XML
            // document has no fixed opening bytes to be told apart by.
            ImageFormat::Svg => false,
        }
    }
}

/// The raster format to draw `filename` as unasked, or `None` to fall through
/// to the normal text/binary handling.
///
/// Both the extension and the signature have to agree. Either alone would be
/// enough to pick a MIME type, but requiring both keeps the two failure modes
/// from turning into misrenderings: a `.png` holding something else is reported
/// as binary rather than handed to the image decoder, and a file that happens
/// to start with a PNG signature is not promoted out of the text view on the
/// strength of its first eight bytes.
pub(super) fn image_format(filename: &str, data: &[u8]) -> Option<ImageFormat> {
    let format = ImageFormat::from_extension(filename)?;
    format.matches_magic(data).then_some(format)
}
