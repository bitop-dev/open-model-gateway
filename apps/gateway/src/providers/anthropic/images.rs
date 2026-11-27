//! Anthropic's limits for inline images (#24).
use crate::providers::image_body::ImageLimits;

/// The Claude API's documented limits (vision documentation): 10 MB of
/// base64 per image, 100 images and 128 MB per request, 8000 px per side.
/// The gateway's own bounds (`protocols::image_input`) are tighter except
/// for the side, which the image's header states.
pub(super) const LIMITS: ImageLimits = ImageLimits {
    per_image: usize::MAX,
    per_image_base64: 10_000_000,
    count: 100,
    side: 8000,
    request_bytes: 128_000_000,
    detail: false,
};
