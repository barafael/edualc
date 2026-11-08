//! Images from outside (the clipboard, the conversation) made safe to show and send. They are
//! decoded in a process of their own, `edualc --decode-image` or `--show-image`, which confines
//! itself before it reads a byte: it reaches no file, socket or other process, runs no new code,
//! and has bounded memory and time. Asked to decode, it answers with the image encoded again
//! (its metadata dropped, scaled to what Claude takes) and a thumbnail; asked to show, with
//! pixels to show. A crafted image can take over that process, which can do no more than answer.
//!
//! The daemon starts it for a paste, the window for an image in the conversation. It is handed
//! the bytes on its standard input and gives one postcard answer on its standard output, read
//! to its end (no framing: the answer is all there is). The answer is untrusted like the image:
//! bounded, parsed as its type, and checked.

use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};

use crate::external::wire::MediaType;

#[cfg(not(target_arch = "wasm32"))]
mod helper;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use helper::{Error, decode, run, show};

/// The most bytes taken to decode.
pub(crate) const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
/// The longest side decoded: Anthropic's vision docs, "The maximum dimensions for each image
/// are 8000x8000 px". Larger is refused from its header, before its pixels are allocated.
const MAX_SIDE_PX: u32 = 8000;
/// The most memory decoding allocates.
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;
/// What the current models take at most; larger is scaled down to fit both. Anthropic's vision
/// docs: "High-resolution | Claude 4.7 and later models | 2,576 px | 4,784" (long edge, visual
/// tokens), "Images larger than either limit are downscaled before processing", and an image
/// "costs `⌈width / 28⌉ × ⌈height / 28⌉` visual tokens".
const SENT_LONG_EDGE_PX: u32 = 2576;
const SENT_VISUAL_TOKENS: u32 = 4784;
const VISUAL_TOKEN_PX: u32 = 28;
/// The most an image sent may weigh: base64 makes 4 of every 3 bytes, and Anthropic's vision
/// docs say "5 MB (base64-encoded) on Amazon Bedrock" (10 MB on the Claude API).
const MAX_SENT_BYTES: usize = 5 * 1000 * 1000 / 4 * 3;
/// The longest side of the thumbnail shown in the composer.
pub(crate) const THUMBNAIL_SIDE_PX: u32 = 192;
/// The longest side of an image shown in the conversation: twice the 480 px it takes there at
/// most, for a display scaled 2×.
pub(crate) const SHOWN_SIDE_PX: u32 = 960;
/// The most the helper may answer when asked to show.
const MAX_SHOWN_ANSWER_BYTES: usize = 4 * (SHOWN_SIDE_PX as usize).pow(2) + 1024;
/// The most the helper may answer, and so the most a pasted image comes to in the window.
pub(crate) const MAX_ANSWER_BYTES: usize = MAX_SENT_BYTES + 1024 * 1024;

/// The image formats read, as a clipboard offers them: Anthropic's vision docs, "Claude
/// supports JPEG, PNG, GIF, and WebP".
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::AsRefStr)]
pub(crate) enum Format {
    #[strum(serialize = "image/png")]
    Png,
    #[strum(serialize = "image/jpeg")]
    Jpeg,
    #[strum(serialize = "image/gif")]
    Gif,
    #[strum(serialize = "image/webp")]
    WebP,
}

/// What the helper is asked to make of an image, by the argument it is started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::AsRefStr)]
pub(crate) enum Job {
    /// To send: encoded again for Claude, with a thumbnail for the composer ([`Decoded`]).
    #[strum(serialize = "--decode-image")]
    Send,
    /// To show in the conversation: pixels, at most [`SHOWN_SIDE_PX`] on a side ([`Shown`]).
    #[strum(serialize = "--show-image")]
    Show,
}

/// What the helper answers when asked to send.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Decoded {
    Image(Picture),
    Refused(Refusal),
}

/// What the helper answers when asked to show.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Shown {
    Image(Pixels),
    Refused(Refusal),
}

/// An image to send, and its thumbnail to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Picture {
    pub(crate) media_type: MediaType,
    pub(crate) bytes: Vec<u8>,
    pub(crate) width_px: u32,
    pub(crate) height_px: u32,
    pub(crate) thumbnail: Pixels,
}

/// Pixels, four bytes each (red, green, blue, alpha), row by row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pixels {
    pub(crate) width_px: u32,
    pub(crate) height_px: u32,
    pub(crate) rgba: Vec<u8>,
}

impl Pixels {
    /// Whether there are as many bytes as pixels, four each.
    pub(crate) fn is_whole(&self) -> bool {
        let pixels = u64::from(self.width_px) * u64::from(self.height_px);
        u64::try_from(self.rgba.len()).ok() == Some(pixels * 4)
    }
}

/// Why the helper wouldn't make an image of what it was given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub(crate) enum Refusal {
    #[error("more than {MAX_INPUT_BYTES} bytes")]
    TooManyBytes,
    #[error("not a PNG, JPEG, GIF or WebP image")]
    UnknownFormat,
    #[error("larger than {MAX_SIDE_PX}×{MAX_SIDE_PX} pixels, or more than decoding may take")]
    TooLarge,
    #[error("the image doesn't decode: {0}")]
    Undecodable(String),
    #[error("couldn't encode the image again: {0}")]
    Unencodable(String),
    #[error("more than {MAX_SENT_BYTES} bytes, even as a JPEG")]
    TooHeavy,
}

/// What the helper makes of an image's bytes (sans-io: it runs in the helper, and in tests).
fn decoded(bytes: &[u8], max_side_px: u32) -> Result<Picture, Refusal> {
    let (image, came_as) = read(bytes, max_side_px)?;
    let (width_px, height_px) = sent_size(image.width(), image.height());
    let sent = if (width_px, height_px) == (image.width(), image.height()) {
        image
    } else {
        image.resize_exact(width_px, height_px, FilterType::Lanczos3)
    };
    let thumbnail = sent
        .thumbnail(THUMBNAIL_SIDE_PX, THUMBNAIL_SIDE_PX)
        .to_rgba8();
    let (media_type, bytes) = encoded(&sent, came_as)?;
    Ok(Picture {
        media_type,
        bytes,
        width_px,
        height_px,
        thumbnail: Pixels {
            width_px: thumbnail.width(),
            height_px: thumbnail.height(),
            rgba: thumbnail.into_raw(),
        },
    })
}

/// What the helper makes of an image's bytes to show: its pixels, scaled down to fit
/// [`SHOWN_SIDE_PX`] (sans-io, as [`decoded`]). The browser demo calls it in the page.
pub(crate) fn shown(bytes: &[u8]) -> Result<Pixels, Refusal> {
    let (image, _came_as) = read(bytes, MAX_SIDE_PX)?;
    let image = if image.width().max(image.height()) > SHOWN_SIDE_PX {
        image.resize(SHOWN_SIDE_PX, SHOWN_SIDE_PX, FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Ok(Pixels {
        width_px: rgba.width(),
        height_px: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// An image read from its bytes, within the limits on its size and on what decoding it takes;
/// and how it is to be sent. A GIF or WebP goes as a PNG, which keeps its pixels (an
/// animation's first frame, as decoding gives it).
fn read(bytes: &[u8], max_side_px: u32) -> Result<(DynamicImage, MediaType), Refusal> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| Refusal::Undecodable(error.to_string()))?;
    let came_as = match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP) => MediaType::Png,
        Some(ImageFormat::Jpeg) => MediaType::Jpeg,
        Some(_) | None => return Err(Refusal::UnknownFormat),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_side_px);
    limits.max_image_height = Some(max_side_px);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader.decode().map_err(|error| match error {
        image::ImageError::Limits(_) => Refusal::TooLarge,
        other @ (image::ImageError::Decoding(_)
        | image::ImageError::Encoding(_)
        | image::ImageError::Parameter(_)
        | image::ImageError::Unsupported(_)
        | image::ImageError::IoError(_)) => Refusal::Undecodable(other.to_string()),
    })?;
    Ok((image, came_as))
}

/// The image encoded as it came (a PNG stays exact); one too heavy that way goes as a JPEG.
fn encoded(image: &DynamicImage, came_as: MediaType) -> Result<(MediaType, Vec<u8>), Refusal> {
    let mut tries = vec![came_as];
    if came_as == MediaType::Png {
        tries.push(MediaType::Jpeg);
    }
    for media_type in tries {
        let mut bytes = Cursor::new(Vec::new());
        let written = match media_type {
            MediaType::Png => image.write_to(&mut bytes, ImageFormat::Png),
            // JPEG has no alpha.
            MediaType::Jpeg => {
                DynamicImage::ImageRgb8(image.to_rgb8()).write_to(&mut bytes, ImageFormat::Jpeg)
            }
        };
        written.map_err(|error| Refusal::Unencodable(error.to_string()))?;
        let bytes = bytes.into_inner();
        if bytes.len() <= MAX_SENT_BYTES {
            return Ok((media_type, bytes));
        }
    }
    Err(Refusal::TooHeavy)
}

/// The largest size, the aspect kept, within both of the current models' limits.
fn sent_size(width_px: u32, height_px: u32) -> (u32, u32) {
    let tokens = |width: u32, height: u32| {
        width.div_ceil(VISUAL_TOKEN_PX) * height.div_ceil(VISUAL_TOKEN_PX)
    };
    let long = width_px.max(height_px);
    if long <= SENT_LONG_EDGE_PX && tokens(width_px, height_px) <= SENT_VISUAL_TOKENS {
        return (width_px, height_px);
    }
    let scaled = |numerator: u64, denominator: u64| {
        let side = |side_px: u32| {
            let side = u64::from(side_px) * numerator / denominator;
            u32::try_from(side).unwrap_or(u32::MAX).max(1)
        };
        (side(width_px), side(height_px))
    };
    // The long edge first, then smaller steps while the tokens don't fit.
    let mut numerator = u64::from(SENT_LONG_EDGE_PX.min(long));
    let denominator = u64::from(long);
    let mut size = scaled(numerator, denominator);
    while tokens(size.0, size.1) > SENT_VISUAL_TOKENS && numerator > 1 {
        numerator -= 1;
        size = scaled(numerator, denominator);
    }
    size
}
