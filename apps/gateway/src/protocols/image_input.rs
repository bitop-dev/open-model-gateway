//! Inline image input on Chat Completions and Responses (#24): base64 data
//! URLs of PNG, JPEG, WebP and GIF only. A remote URL or a file reference is
//! refused before routing: the gateway never fetches a caller's URL and
//! never asks a provider to.
//!
//! Each image is bounded before it is decoded (its base64 length against
//! [`MAX_DECODED_BYTES`], the request's count against [`MAX_IMAGES`]), then
//! decoded once into a scratch buffer that checks strict base64, the
//! declared type's magic bytes, the decoded size and, when its header states
//! them, its pixel dimensions (`dimensions`), and is dropped at once.
//! The data URL itself moves into the [`ImageInput`] without a copy.
use base64::Engine as _;

mod dimensions;

use crate::inference::{
    error::InferenceError,
    types::{ImageDetail, ImageInput, ImageType},
};

/// Images per request.
pub const MAX_IMAGES: usize = 20;
/// Decoded bytes per image (as native Messages images).
pub const MAX_DECODED_BYTES: usize = 5 * 1024 * 1024;
/// The base64 length of [`MAX_DECODED_BYTES`].
pub(crate) const MAX_BASE64_BYTES: usize = MAX_DECODED_BYTES.div_ceil(3) * 4;

/// The images one request has taken so far.
#[derive(Default)]
pub struct Images(usize);

impl Images {
    /// One more image of this request, validated from its URL. `field` names
    /// the field in every refusal; nothing of the URL's value is echoed.
    pub fn take(
        &mut self,
        url: String,
        detail: Option<&str>,
        field: &'static str,
    ) -> Result<ImageInput, InferenceError> {
        let invalid = InferenceError::InvalidField(field);
        let detail = match detail {
            None => None,
            Some(value) => Some(ImageDetail::parse(value).ok_or(invalid)?),
        };
        if self.0 >= MAX_IMAGES {
            return Err(invalid);
        }
        let media_type = ImageType::ALL
            .into_iter()
            .find(|t| url.starts_with(t.data_url_prefix()))
            .ok_or(invalid)?;
        let data = &url[media_type.data_url_prefix().len()..];
        if data.is_empty() || data.len() > MAX_BASE64_BYTES {
            return Err(invalid);
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| invalid)?;
        if decoded.len() > MAX_DECODED_BYTES || !media_type.matches(&decoded) {
            return Err(invalid);
        }
        let decoded_bytes = decoded.len();
        let size = dimensions::dimensions(media_type, &decoded);
        drop(decoded);
        self.0 += 1;
        Ok(ImageInput::new(
            url,
            media_type,
            detail,
            decoded_bytes,
            size,
        ))
    }
}

/// Chat Completions message content: a string, or an array of parts.
/// Deserialized by hand: serde's untagged and internally tagged enums buffer
/// their input, which would copy every image a second time.
pub enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

/// One Chat Completions content part (`text` or `image_url`), checked by
/// [`WirePart::kind`] after deserialization. `file` and `input_audio` parts
/// are read only to be refused by name.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WirePart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
    pub image_url: Option<WireImageUrl>,
    pub file: Option<serde::de::IgnoredAny>,
    pub input_audio: Option<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireImageUrl {
    pub url: String,
    pub detail: Option<String>,
}

impl<'de> serde::Deserialize<'de> for WireContent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = WireContent;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or an array of content parts")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<WireContent, E> {
                Ok(WireContent::Text(v.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<WireContent, E> {
                Ok(WireContent::Text(v))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<WireContent, A::Error> {
                let mut parts = Vec::new();
                while let Some(part) = seq.next_element::<WirePart>()? {
                    if parts.len() >= 256 {
                        return Err(serde::de::Error::custom("too many content parts"));
                    }
                    parts.push(part);
                }
                Ok(WireContent::Parts(parts))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn url(media: &str, bytes: &[u8]) -> String {
        format!(
            "data:{media};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    #[test]
    fn accepts_each_type_by_its_magic_bytes() {
        let webp = b"RIFF\0\0\0\0WEBPVP8 ";
        for (media, bytes) in [
            ("image/png", PNG),
            ("image/jpeg", &[0xff, 0xd8, 0xff, 0xe0][..]),
            ("image/gif", b"GIF89a\x01\0"),
            ("image/webp", webp),
        ] {
            let image = Images::default()
                .take(url(media, bytes), Some("low"), "image")
                .unwrap_or_else(|_| panic!("{media}"));
            assert_eq!(image.media_type().mime(), media);
            assert_eq!(image.decoded_bytes(), bytes.len());
            assert_eq!(image.detail(), Some(ImageDetail::Low));
            assert_eq!(image.data_url(), url(media, bytes));
            assert_eq!(
                image.base64(),
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
        }
    }

    #[test]
    fn refuses_mismatched_remote_malformed_and_oversized_images() {
        let too_big = {
            let mut b = PNG.to_vec();
            b.resize(MAX_DECODED_BYTES + 1, 0);
            b
        };
        for (case, value) in [
            ("declared jpeg, png bytes", url("image/jpeg", PNG)),
            ("unsupported type", url("image/bmp", PNG)),
            ("svg", url("image/svg+xml", b"<svg/>")),
            ("https url", "https://example.invalid/cat.png".into()),
            ("http url", "http://127.0.0.1/cat.png".into()),
            (
                "no base64 marker",
                format!("data:image/png,{}", "iVBORw0KGgo"),
            ),
            (
                "parameters",
                format!("data:image/png;charset=x;base64,{}", "iVBORw0KGgo"),
            ),
            ("empty", "data:image/png;base64,".into()),
            ("not base64", "data:image/png;base64,***".into()),
            ("unpadded", "data:image/png;base64,iVBORw0KGgo".into()),
            (
                "whitespace",
                "data:image/png;base64,iVBO Rw0KGgoAAAA=".into(),
            ),
            ("over the decoded cap", url("image/png", &too_big)),
            (
                "uppercase scheme",
                url("image/png", PNG).replace("data:", "DATA:"),
            ),
        ] {
            let error = Images::default().take(value, None, "image_url.url").err();
            assert_eq!(
                error,
                Some(InferenceError::InvalidField("image_url.url")),
                "{case}"
            );
        }
        assert_eq!(
            Images::default()
                .take(url("image/png", PNG), Some("ultra"), "detail")
                .err(),
            Some(InferenceError::InvalidField("detail"))
        );
    }

    #[test]
    fn the_base64_length_is_bounded_before_decoding() {
        // Over the base64 bound but not valid base64: refused by length,
        // which a decode would also refuse, so check the bound directly.
        let long = format!("data:image/png;base64,{}", "A".repeat(MAX_BASE64_BYTES + 4));
        assert!(Images::default().take(long, None, "image").is_err());
        assert_eq!(MAX_BASE64_BYTES, 6_990_508);
    }

    #[test]
    fn at_most_twenty_images_per_request() {
        let mut images = Images::default();
        for _ in 0..MAX_IMAGES {
            images.take(url("image/png", PNG), None, "image").unwrap();
        }
        assert_eq!(
            images.take(url("image/png", PNG), None, "image").err(),
            Some(InferenceError::InvalidField("image"))
        );
    }

    #[test]
    fn content_is_a_string_or_parts() {
        let text: WireContent = serde_json::from_str(r#""hi""#).unwrap();
        assert!(matches!(text, WireContent::Text(t) if t == "hi"));
        let parts: WireContent = serde_json::from_str(
            r#"[{"type":"text","text":"a"},{"type":"image_url","image_url":{"url":"data:x","detail":"low"}}]"#,
        )
        .unwrap();
        let WireContent::Parts(parts) = parts else {
            panic!("parts")
        };
        assert_eq!(parts.len(), 2);
        assert!(serde_json::from_str::<WireContent>("42").is_err());
        assert!(
            serde_json::from_str::<WireContent>(r#"[{"type":"text","text":"a","extra":1}]"#)
                .is_err()
        );
    }
}
