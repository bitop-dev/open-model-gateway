//! Converse image blocks for inline images (#24). Converse takes an image's
//! raw bytes (the SDK writes them as base64 in the request body), so each
//! image is decoded once more, at encoding; the pre-admission dry run
//! (`validate_chat_request`) checks the same limits without decoding, which
//! cannot fail for an image `Images::take` already decoded.
use aws_sdk_bedrockruntime::types as aws;
use aws_smithy_types::Blob;
use base64::Engine as _;

use crate::{
    inference::{
        error::InferenceError,
        types::{ImageType, Part},
    },
    providers::image_body::ImageLimits,
};

/// Bedrock's limits for Converse images: 3.75 MB, 8000 px a side and 20
/// images (API reference, `Message.content`), and 20 MB per request
/// (Anthropic's documentation of Claude on Bedrock). Megabytes are taken as
/// 10^6 bytes, the smaller reading.
pub(super) const LIMITS: ImageLimits = ImageLimits {
    per_image: 3_750_000,
    per_image_base64: usize::MAX,
    count: 20,
    side: 8000,
    request_bytes: 20_000_000,
    detail: false,
};

/// Text and image blocks of content with parts, in order. With `decode`
/// false (validation only) an image's bytes are left empty.
pub(super) fn blocks(
    parts: &[Part],
    decode: bool,
) -> Result<Vec<aws::ContentBlock>, InferenceError> {
    let refuse = || InferenceError::InvalidField("image");
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            Part::Text(text) if text.is_empty() => {}
            Part::Text(text) => blocks.push(aws::ContentBlock::Text(text.clone())),
            Part::Image(image) => {
                let bytes = if decode {
                    base64::engine::general_purpose::STANDARD
                        .decode(image.base64())
                        .map_err(|_| refuse())?
                } else {
                    Vec::new()
                };
                let block = aws::ImageBlock::builder()
                    .format(format(image.media_type()))
                    .source(aws::ImageSource::Bytes(Blob::new(bytes)))
                    .build()
                    .map_err(|_| refuse())?;
                blocks.push(aws::ContentBlock::Image(block));
            }
        }
    }
    Ok(blocks)
}

fn format(kind: ImageType) -> aws::ImageFormat {
    match kind {
        ImageType::Png => aws::ImageFormat::Png,
        ImageType::Jpeg => aws::ImageFormat::Jpeg,
        ImageType::Webp => aws::ImageFormat::Webp,
        ImageType::Gif => aws::ImageFormat::Gif,
    }
}
