//! Images.
//! This module has some helpers for modeling terminal cells that are filled
//! with image data.
//! We're targeting the iTerm image protocol initially, with sixel as an obvious
//! follow up.
//! Kitty has an extensive and complex graphics protocol
//! whose docs are here:
//! <https://github.com/kovidgoyal/kitty/blob/master/docs/graphics-protocol.rst>
//! Both iTerm2 and Sixel appear to have semantics that allow replacing the
//! contents of a single chararcter cell with image data, whereas the kitty
//! protocol appears to track the images out of band as attachments with
//! z-order.

use ordered_float::NotNan;
#[cfg(feature = "use_serde")]
use serde::de::{SeqAccess, Visitor};
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::any::Any;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use wezterm_blob_leases::{BlobLease, BlobManager};
use wezterm_runtime_admission::MAX_WIRE_BYTE_BUFFER_BYTES;

#[cfg(feature = "use_image")]
const MAX_DECODED_IMAGE_BYTES: usize = 100_000_000;
#[cfg(feature = "use_image")]
const MAX_IMAGE_DECODE_WORKING_BYTES: usize = 268_435_456;
#[cfg(feature = "use_image")]
const MAX_IMAGE_ANIMATION_FRAMES: usize = 4_096;

#[cfg(feature = "use_image")]
fn checked_rgba_image_bytes(width: u32, height: u32) -> Result<usize, &'static str> {
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("decoded image dimensions overflow")?;
    if bytes == 0 {
        return Err("decoded image has zero dimensions");
    }
    if bytes > MAX_DECODED_IMAGE_BYTES {
        return Err("decoded image exceeds the retained byte limit");
    }
    Ok(bytes)
}

#[cfg(feature = "use_image")]
fn image_decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    let max_dimension = (MAX_DECODED_IMAGE_BYTES / 4) as u32;
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_WORKING_BYTES as u64);
    limits
}

#[cfg(feature = "use_serde")]
fn deserialize_notnan<'de, D>(deserializer: D) -> Result<NotNan<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f32::deserialize(deserializer)?;
    NotNan::new(value).map_err(|e| serde::de::Error::custom(format!("{:?}", e)))
}

#[cfg(feature = "use_serde")]
#[allow(clippy::trivially_copy_pass_by_ref)]
fn serialize_notnan<S>(value: &NotNan<f32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    value.into_inner().serialize(serializer)
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureCoordinate {
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_notnan",
            serialize_with = "serialize_notnan"
        )
    )]
    pub x: NotNan<f32>,
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_notnan",
            serialize_with = "serialize_notnan"
        )
    )]
    pub y: NotNan<f32>,
}

impl TextureCoordinate {
    pub fn new(x: NotNan<f32>, y: NotNan<f32>) -> Self {
        Self { x, y }
    }

    pub fn new_f32(x: f32, y: f32) -> Self {
        let x = NotNan::new(x).unwrap();
        let y = NotNan::new(y).unwrap();
        Self::new(x, y)
    }
}

/// Tracks data for displaying an image in the place of the normal cell
/// character data.  Since an Image can span multiple cells, we need to logically
/// carve up the image and track each slice of it.  Each cell needs to know
/// its "texture coordinates" within that image so that we can render the
/// right slice.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCell {
    /// Texture coordinate for the top left of this cell.
    /// (0,0) is the top left of the ImageData. (1, 1) is
    /// the bottom right.
    top_left: TextureCoordinate,
    /// Texture coordinates for the bottom right of this cell.
    bottom_right: TextureCoordinate,
    /// References the underlying image data
    data: Arc<ImageData>,
    z_index: i32,
    /// When rendering in the cell, use this offset from the top left
    /// of the cell
    padding_left: u16,
    padding_top: u16,
    padding_right: u16,
    padding_bottom: u16,

    image_id: Option<u32>,
    placement_id: Option<u32>,
}

impl ImageCell {
    pub fn new(
        top_left: TextureCoordinate,
        bottom_right: TextureCoordinate,
        data: Arc<ImageData>,
    ) -> Self {
        Self::with_z_index(top_left, bottom_right, data, 0, 0, 0, 0, 0, None, None)
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.top_left.hash(hasher);
        self.bottom_right.hash(hasher);
        self.data.hash.hash(hasher);
        self.z_index.hash(hasher);
        self.padding_left.hash(hasher);
        self.padding_top.hash(hasher);
        self.padding_right.hash(hasher);
        self.padding_bottom.hash(hasher);
        self.image_id.hash(hasher);
        self.placement_id.hash(hasher);
    }

    pub fn with_z_index(
        top_left: TextureCoordinate,
        bottom_right: TextureCoordinate,
        data: Arc<ImageData>,
        z_index: i32,
        padding_left: u16,
        padding_top: u16,
        padding_right: u16,
        padding_bottom: u16,
        image_id: Option<u32>,
        placement_id: Option<u32>,
    ) -> Self {
        Self {
            top_left,
            bottom_right,
            data,
            z_index,
            padding_left,
            padding_top,
            padding_right,
            padding_bottom,
            image_id,
            placement_id,
        }
    }

    pub fn matches_placement(&self, image_id: u32, placement_id: Option<u32>) -> bool {
        self.image_id == Some(image_id) && self.placement_id == placement_id
    }

    pub fn has_placement_id(&self) -> bool {
        self.placement_id.is_some()
    }

    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    pub fn placement_id(&self) -> Option<u32> {
        self.placement_id
    }

    pub fn top_left(&self) -> TextureCoordinate {
        self.top_left
    }

    pub fn bottom_right(&self) -> TextureCoordinate {
        self.bottom_right
    }

    pub fn image_data(&self) -> &Arc<ImageData> {
        &self.data
    }

    /// negative z_index is rendered beneath the text layer.
    /// >= 0 is rendered above the text.
    /// negative z_index < INT32_MIN/2 will be drawn under cells
    /// with non-default background colors
    pub fn z_index(&self) -> i32 {
        self.z_index
    }

    /// Returns padding (left, top, right, bottom)
    pub fn padding(&self) -> (u16, u16, u16, u16) {
        (
            self.padding_left,
            self.padding_top,
            self.padding_right,
            self.padding_bottom,
        )
    }
}

/// Native image bytes admitted under the same bound as a wire byte buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedBlobBytes(Vec<u8>);

impl BoundedBlobBytes {
    pub fn new(data: Vec<u8>) -> Result<Self, EncodedBlobError> {
        if data.len() > MAX_WIRE_BYTE_BUFFER_BYTES {
            return Err(EncodedBlobError::TooLarge {
                actual: data.len(),
                maximum: MAX_WIRE_BYTE_BUFFER_BYTES,
            });
        }
        Ok(Self(data))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl TryFrom<Vec<u8>> for BoundedBlobBytes {
    type Error = EncodedBlobError;

    fn try_from(data: Vec<u8>) -> Result<Self, Self::Error> {
        Self::new(data)
    }
}

#[cfg(feature = "use_serde")]
impl Serialize for BoundedBlobBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for BoundedBlobBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedBlobBytesVisitor;

        impl<'de> Visitor<'de> for BoundedBlobBytesVisitor {
            type Value = BoundedBlobBytes;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    formatter,
                    "at most {MAX_WIRE_BYTE_BUFFER_BYTES} encoded image bytes"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let hinted = sequence.size_hint().unwrap_or(0);
                if hinted > MAX_WIRE_BYTE_BUFFER_BYTES {
                    return Err(serde::de::Error::custom(format_args!(
                        "encoded image payload of {hinted} bytes exceeds maximum of \
                         {MAX_WIRE_BYTE_BUFFER_BYTES}"
                    )));
                }

                let mut data = Vec::with_capacity(hinted);
                while let Some(byte) = sequence.next_element()? {
                    if data.len() == MAX_WIRE_BYTE_BUFFER_BYTES {
                        return Err(serde::de::Error::custom(format_args!(
                            "encoded image payload exceeds maximum of \
                             {MAX_WIRE_BYTE_BUFFER_BYTES} bytes"
                        )));
                    }
                    data.push(byte);
                }
                Ok(BoundedBlobBytes(data))
            }
        }

        deserializer.deserialize_seq(BoundedBlobBytesVisitor)
    }
}

/// Encoded image data has one wire representation: bounded inline bytes.
///
/// `Stored` is an in-process retained-state optimization. Serialization reads
/// those bytes, while deserialization always yields `Inline`; decoding never
/// writes to blob storage.
#[derive(Clone, PartialEq, Eq)]
pub enum EncodedBlob {
    Inline(BoundedBlobBytes),
    Stored(BlobLease),
}

impl EncodedBlob {
    pub fn inline(data: Vec<u8>) -> Result<Self, EncodedBlobError> {
        Ok(Self::Inline(BoundedBlobBytes::new(data)?))
    }

    fn compute_hash(&self) -> [u8; 32] {
        match self {
            Self::Inline(data) => ImageDataType::hash_bytes(data.as_slice()),
            Self::Stored(lease) => lease.content_id().as_hash_bytes(),
        }
    }
}

impl fmt::Debug for EncodedBlob {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Inline(data) => formatter
                .debug_struct("Inline")
                .field("data_of_len", &data.len())
                .finish(),
            Self::Stored(lease) => formatter.debug_tuple("Stored").field(lease).finish(),
        }
    }
}

#[cfg(feature = "use_serde")]
impl Serialize for EncodedBlob {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Inline(data) => data.serialize(serializer),
            Self::Stored(lease) => {
                let data = lease
                    .read_with_limit(MAX_WIRE_BYTE_BUFFER_BYTES)
                    .map_err(|error| serde::ser::Error::custom(format_args!("{error:#}")))?;
                data.as_slice().serialize(serializer)
            }
        }
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for EncodedBlob {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        BoundedBlobBytes::deserialize(deserializer).map(Self::Inline)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum EncodedBlobError {
    #[error("encoded image payload of {actual} bytes exceeds maximum of {maximum}")]
    TooLarge { actual: usize, maximum: usize },
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Clone, PartialEq, Eq)]
pub enum ImageDataType {
    /// Data in its native image file format. Animated formats stay encoded.
    Encoded(EncodedBlob),
    /// Data is RGBA u8 data
    Rgba8 {
        data: Vec<u8>,
        width: u32,
        height: u32,
        hash: [u8; 32],
    },
    /// Data is an animated sequence
    AnimRgba8 {
        width: u32,
        height: u32,
        durations: Vec<Duration>,
        frames: Vec<Vec<u8>>,
        hashes: Vec<[u8; 32]>,
    },
}

impl std::fmt::Debug for ImageDataType {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Encoded(data) => data.fmt(fmt),
            Self::Rgba8 {
                data,
                width,
                height,
                hash,
            } => fmt
                .debug_struct("Rgba8")
                .field("data_of_len", &data.len())
                .field("width", &width)
                .field("height", &height)
                .field("hash", &hash)
                .finish(),
            Self::AnimRgba8 {
                frames,
                width,
                height,
                durations,
                hashes,
            } => fmt
                .debug_struct("AnimRgba8")
                .field("frames_of_len", &frames.len())
                .field("width", &width)
                .field("height", &height)
                .field("durations", durations)
                .field("hashes", hashes)
                .finish(),
        }
    }
}

impl ImageDataType {
    pub fn new_single_frame(width: u32, height: u32, data: Vec<u8>) -> Self {
        let hash = Self::hash_bytes(&data);
        assert_eq!(
            width * height * 4,
            data.len() as u32,
            "invalid dimensions {}x{} for pixel data of length {}",
            width,
            height,
            data.len()
        );
        Self::Rgba8 {
            width,
            height,
            data,
            hash,
        }
    }

    /// Black pixels
    pub fn placeholder() -> Self {
        let mut data = vec![];
        let size = 8;
        for _ in 0..size * size {
            data.extend_from_slice(&[0, 0, 0, 0xff]);
        }
        ImageDataType::new_single_frame(size, size, data)
    }

    pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(bytes);
        hasher.finalize().into()
    }

    pub fn compute_hash(&self) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        match self {
            ImageDataType::Encoded(data) => return data.compute_hash(),
            ImageDataType::Rgba8 { data, .. } => hasher.update(data),
            ImageDataType::AnimRgba8 {
                frames, durations, ..
            } => {
                for data in frames {
                    hasher.update(data);
                }
                for d in durations {
                    let d = d.as_secs_f32();
                    let b = d.to_ne_bytes();
                    hasher.update(b);
                }
            }
        };
        hasher.finalize().into()
    }

    /// Divides the animation frame durations by the provided
    /// speed_factor, so a factor of 2 will halve the duration.
    /// # Panics
    /// if the speed_factor is negative, non-finite or the result
    /// overflows the allow Duration range.
    pub fn adjust_speed(&mut self, speed_factor: f32) {
        match self {
            Self::AnimRgba8 { durations, .. } => {
                for d in durations {
                    *d = d.mul_f32(1. / speed_factor);
                }
            }
            _ => {}
        }
    }

    #[cfg(feature = "use_image")]
    pub fn dimensions(&self) -> Result<(u32, u32), ImageCellError> {
        fn dimensions_for_data(data: &[u8]) -> image::ImageResult<(u32, u32)> {
            let mut reader =
                image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
            reader.limits(image_decode_limits());
            let (width, height) = reader.into_dimensions()?;
            checked_rgba_image_bytes(width, height).map_err(|_| {
                image::ImageError::Limits(image::error::LimitError::from_kind(
                    image::error::LimitErrorKind::InsufficientMemory,
                ))
            })?;

            Ok((width, height))
        }

        match self {
            ImageDataType::Encoded(EncodedBlob::Inline(data)) => {
                Ok(dimensions_for_data(data.as_slice())?)
            }
            ImageDataType::Encoded(EncodedBlob::Stored(lease)) => {
                let data = lease.read_with_limit(MAX_WIRE_BYTE_BUFFER_BYTES)?;
                Ok(dimensions_for_data(data.as_slice())?)
            }
            ImageDataType::AnimRgba8 { width, height, .. }
            | ImageDataType::Rgba8 { width, height, .. } => Ok((*width, *height)),
        }
    }

    /// Migrate an in-memory encoded image blob to on-disk to reduce
    /// the memory footprint
    pub fn swap_out(self) -> Result<Self, ImageCellError> {
        match self {
            Self::Encoded(EncodedBlob::Inline(data)) => match BlobManager::store(data.as_slice()) {
                Ok(lease) => Ok(Self::Encoded(EncodedBlob::Stored(lease))),
                Err(wezterm_blob_leases::Error::StorageNotInit) => {
                    Ok(Self::Encoded(EncodedBlob::Inline(data)))
                }
                Err(err) => Err(err.into()),
            },
            other => Ok(other),
        }
    }

    /// Decode inline bytes into either an Rgba8 or AnimRgba8 variant
    /// if we recognize the file format, otherwise the encoded data
    /// is preserved as is.
    #[cfg(feature = "use_image")]
    pub fn decode(self) -> Self {
        use image::{AnimationDecoder, ImageDecoder, ImageFormat};

        match self {
            Self::Encoded(EncodedBlob::Inline(data)) => {
                let data = data.into_vec();
                let format = match image::guess_format(&data) {
                    Ok(format) => format,
                    Err(err) => {
                        log::warn!("Unable to decode raw image data: {:#}", err);
                        return Self::Encoded(
                            EncodedBlob::inline(data).expect("previously bounded"),
                        );
                    }
                };
                let cursor = std::io::Cursor::new(&*data);
                match format {
                    ImageFormat::Gif => {
                        let decoded = image::codecs::gif::GifDecoder::new(cursor)
                            .map_err(|error| error.to_string())
                            .and_then(|mut decoder| {
                                let (width, height) = decoder.dimensions();
                                checked_rgba_image_bytes(width, height)
                                    .map_err(|error| error.to_string())?;
                                decoder
                                    .set_limits(image_decode_limits())
                                    .map_err(|error| error.to_string())?;
                                Self::decode_frames(decoder.into_frames())
                            });
                        decoded.unwrap_or_else(|err| {
                            log::error!(
                                "Unable to parse animated gif: {:#}, trying as single frame",
                                err
                            );
                            Self::decode_single(data)
                        })
                    }
                    ImageFormat::Png => {
                        let decoder = match image::codecs::png::PngDecoder::with_limits(
                            cursor,
                            image_decode_limits(),
                        ) {
                            Ok(d) => d,
                            _ => {
                                return Self::Encoded(
                                    EncodedBlob::inline(data).expect("previously bounded"),
                                );
                            }
                        };
                        let (width, height) = decoder.dimensions();
                        if checked_rgba_image_bytes(width, height).is_err() {
                            drop(decoder);
                            return Self::Encoded(
                                EncodedBlob::inline(data).expect("previously bounded"),
                            );
                        }
                        if decoder.is_apng().unwrap_or(false) {
                            match decoder.apng() {
                                Ok(decoder) => match Self::decode_frames(decoder.into_frames()) {
                                    Ok(decoded) => decoded,
                                    Err(error) => {
                                        log::error!("Unable to decode APNG: {error}");
                                        Self::Encoded(
                                            EncodedBlob::inline(data).expect("previously bounded"),
                                        )
                                    }
                                },
                                Err(_) => Self::Encoded(
                                    EncodedBlob::inline(data).expect("previously bounded"),
                                ),
                            }
                        } else {
                            drop(decoder);
                            Self::decode_single(data)
                        }
                    }
                    ImageFormat::WebP => {
                        let mut decoder = match image::codecs::webp::WebPDecoder::new(cursor) {
                            Ok(d) => d,
                            _ => {
                                return Self::Encoded(
                                    EncodedBlob::inline(data).expect("previously bounded"),
                                );
                            }
                        };
                        let (width, height) = decoder.dimensions();
                        if checked_rgba_image_bytes(width, height).is_err()
                            || decoder.set_limits(image_decode_limits()).is_err()
                        {
                            drop(decoder);
                            return Self::Encoded(
                                EncodedBlob::inline(data).expect("previously bounded"),
                            );
                        }
                        match Self::decode_frames(decoder.into_frames()) {
                            Ok(decoded) => decoded,
                            Err(error) => {
                                log::error!("Unable to decode animated WebP: {error}");
                                Self::Encoded(
                                    EncodedBlob::inline(data).expect("previously bounded"),
                                )
                            }
                        }
                    }
                    _ => Self::decode_single(data),
                }
            }
            data => data,
        }
    }

    #[cfg(not(feature = "use_image"))]
    pub fn decode(self) -> Self {
        self
    }

    #[cfg(feature = "use_image")]
    fn decode_frames<I>(mut img_frames: I) -> Result<Self, String>
    where
        I: Iterator<Item = image::ImageResult<image::Frame>>,
    {
        let (minimum_frames, maximum_frames) = img_frames.size_hint();
        if minimum_frames > MAX_IMAGE_ANIMATION_FRAMES
            || maximum_frames.map_or(false, |maximum| maximum > MAX_IMAGE_ANIMATION_FRAMES)
        {
            return Err("animation frame count hint exceeds limit".to_string());
        }
        let mut width = 0;
        let mut height = 0;
        let mut frames = vec![];
        let mut durations = vec![];
        let mut hashes = vec![];
        let mut retained_frame_bytes = 0usize;
        while let Some(frame) = img_frames.next() {
            if frames.len() >= MAX_IMAGE_ANIMATION_FRAMES {
                return Err("animation frame count exceeds limit".to_string());
            }
            let frame = frame.map_err(|error| error.to_string())?;
            let duration: Duration = frame.delay().into();
            let image = frame.into_buffer();
            let (w, h) = image.dimensions();
            let frame_bytes = checked_rgba_image_bytes(w, h).map_err(|error| error.to_string())?;
            retained_frame_bytes = retained_frame_bytes
                .checked_add(frame_bytes)
                .ok_or_else(|| "animation retained byte count overflow".to_string())?;
            if retained_frame_bytes > MAX_DECODED_IMAGE_BYTES {
                return Err("animation retained bytes exceed limit".to_string());
            }
            if !frames.is_empty() && (width != w || height != h) {
                return Err("animation frame dimensions are inconsistent".to_string());
            }
            width = w;
            height = h;
            let data = image.into_vec();
            if data.len() != frame_bytes {
                return Err("animation frame allocation does not match dimensions".to_string());
            }
            durations.push(duration);
            hashes.push(Self::hash_bytes(&data));
            frames.push(data);
        }
        if frames.is_empty() {
            log::error!("decoded image has 0 frames, using placeholder");
            return Ok(Self::placeholder());
        }
        Ok(Self::AnimRgba8 {
            width,
            height,
            frames,
            durations,
            hashes,
        })
    }

    #[cfg(feature = "use_image")]
    fn decode_single(data: Vec<u8>) -> Self {
        let mut reader = match image::ImageReader::new(std::io::Cursor::new(&*data))
            .with_guessed_format()
        {
            Ok(reader) => reader,
            Err(_) => return Self::Encoded(EncodedBlob::inline(data).expect("previously bounded")),
        };
        reader.limits(image_decode_limits());
        let dimensions = reader.into_dimensions().ok().and_then(|(width, height)| {
            checked_rgba_image_bytes(width, height)
                .ok()
                .map(|_| (width, height))
        });
        let Some((expected_width, expected_height)) = dimensions else {
            return Self::Encoded(EncodedBlob::inline(data).expect("previously bounded"));
        };
        let mut reader = match image::ImageReader::new(std::io::Cursor::new(&*data))
            .with_guessed_format()
        {
            Ok(reader) => reader,
            Err(_) => return Self::Encoded(EncodedBlob::inline(data).expect("previously bounded")),
        };
        reader.limits(image_decode_limits());
        match reader.decode() {
            Ok(image) => {
                let image = image.to_rgba8();
                let (width, height) = image.dimensions();
                if width != expected_width
                    || height != expected_height
                    || checked_rgba_image_bytes(width, height).is_err()
                {
                    return Self::Encoded(EncodedBlob::inline(data).expect("previously bounded"));
                }
                let data = image.into_vec();
                let hash = Self::hash_bytes(&data);
                Self::Rgba8 {
                    width,
                    height,
                    data,
                    hash,
                }
            }
            _ => Self::Encoded(EncodedBlob::inline(data).expect("previously bounded")),
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ImageCellError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    BlobLease(#[from] wezterm_blob_leases::Error),

    #[error(transparent)]
    EncodedBlob(#[from] EncodedBlobError),

    #[error(transparent)]
    ImageError(#[from] image::ImageError),
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct ImageData {
    data: Mutex<ImageDataType>,
    hash: [u8; 32],
    #[cfg_attr(feature = "use_serde", serde(skip, default))]
    retention_guard: Mutex<Option<Box<dyn Any + Send + Sync>>>,
}

struct HexSlice<'a>(&'a [u8]);
impl<'a> std::fmt::Display for HexSlice<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        for byte in self.0 {
            write!(fmt, "{byte:x}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("ImageData")
            .field("data", &self.data)
            .field("hash", &format_args!("{}", HexSlice(&self.hash)))
            .finish()
    }
}

impl Eq for ImageData {}
impl PartialEq for ImageData {
    fn eq(&self, rhs: &Self) -> bool {
        self.hash == rhs.hash
    }
}

impl ImageData {
    /// Create a new ImageData struct with the provided raw data.
    pub fn with_raw_data(data: Vec<u8>) -> Result<Self, EncodedBlobError> {
        let hash = ImageDataType::hash_bytes(&data);
        Ok(Self::with_data_and_hash(
            ImageDataType::Encoded(EncodedBlob::inline(data)?).decode(),
            hash,
        ))
    }

    fn with_data_and_hash(data: ImageDataType, hash: [u8; 32]) -> Self {
        Self {
            data: Mutex::new(data),
            hash,
            retention_guard: Mutex::new(None),
        }
    }

    pub fn with_data(data: ImageDataType) -> Self {
        let hash = data.compute_hash();
        Self {
            data: Mutex::new(data),
            hash,
            retention_guard: Mutex::new(None),
        }
    }

    /// Returns the number of bytes in the image payload.
    pub fn len(&self) -> usize {
        match &*self.data() {
            ImageDataType::Encoded(EncodedBlob::Inline(data)) => data.len(),
            ImageDataType::Encoded(EncodedBlob::Stored(_)) => 0,
            ImageDataType::Rgba8 { data, .. } => data.len(),
            ImageDataType::AnimRgba8 { frames, .. } => frames
                .iter()
                .fold(0usize, |size, frame| size.saturating_add(frame.len())),
        }
    }

    /// Returns the retained in-memory footprint, excluding the optional retention guard.
    pub fn retained_size(&self) -> usize {
        let data = self.data();
        let heap_size = match &*data {
            ImageDataType::Encoded(EncodedBlob::Inline(data)) => data.capacity(),
            ImageDataType::Encoded(EncodedBlob::Stored(_)) => 0,
            ImageDataType::Rgba8 { data, .. } => data.capacity(),
            ImageDataType::AnimRgba8 {
                durations,
                frames,
                hashes,
                ..
            } => {
                let frame_storage = frames
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Vec<u8>>());
                let frame_data = frames
                    .iter()
                    .fold(0usize, |size, frame| size.saturating_add(frame.capacity()));
                frame_storage
                    .saturating_add(frame_data)
                    .saturating_add(
                        durations
                            .capacity()
                            .saturating_mul(core::mem::size_of::<Duration>()),
                    )
                    .saturating_add(
                        hashes
                            .capacity()
                            .saturating_mul(core::mem::size_of::<[u8; 32]>()),
                    )
            }
        };
        core::mem::size_of::<Self>().saturating_add(heap_size)
    }

    /// Installs a value that is retained until the final ImageData owner is dropped.
    pub fn try_set_retention_guard<T: Send + Sync + 'static>(&self, guard: T) -> Result<(), T> {
        let mut slot = self
            .retention_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            Err(guard)
        } else {
            *slot = Some(Box::new(guard));
            Ok(())
        }
    }

    pub fn data(&self) -> MutexGuard<'_, ImageDataType> {
        self.data.lock().unwrap()
    }

    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(feature = "use_image")]
    struct TooManyAnimationFrames;

    #[cfg(feature = "use_image")]
    impl Iterator for TooManyAnimationFrames {
        type Item = image::ImageResult<image::Frame>;

        fn next(&mut self) -> Option<Self::Item> {
            panic!("the frame-count hint must be rejected before decoding a frame")
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let frame_count = MAX_IMAGE_ANIMATION_FRAMES + 1;
            (frame_count, Some(frame_count))
        }
    }

    #[test]
    fn encoded_blob_inline_enforces_the_wire_byte_bound() {
        let data = vec![1, 2, 3];
        let encoded = EncodedBlob::inline(data.clone()).unwrap();
        assert_eq!(
            encoded,
            EncodedBlob::Inline(BoundedBlobBytes::new(data).unwrap())
        );

        let oversized = vec![0; MAX_WIRE_BYTE_BUFFER_BYTES + 1];
        assert!(matches!(
            EncodedBlob::inline(oversized),
            Err(EncodedBlobError::TooLarge { actual, maximum })
                if actual == MAX_WIRE_BYTE_BUFFER_BYTES + 1
                    && maximum == MAX_WIRE_BYTE_BUFFER_BYTES
        ));
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn tiny_png_with_huge_declared_surface_remains_encoded() {
        let png = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0,
            0, 0xff, 0xff, 0, 0, 0xff, 0xff, 8, 6, 0, 0, 0, 0xb6, 0x05, 0xd9, 0x50, 0, 0, 0, 0,
            b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
        ];

        assert!(png.len() < 64);
        let image = ImageDataType::Encoded(EncodedBlob::inline(png).unwrap()).decode();
        assert!(matches!(image, ImageDataType::Encoded(_)));
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn decoded_image_byte_check_rejects_overflow_and_retained_limit() {
        assert!(checked_rgba_image_bytes(u32::MAX, u32::MAX).is_err());
        assert!(checked_rgba_image_bytes(10_000, 10_000).is_err());
        assert_eq!(checked_rgba_image_bytes(1, 1).unwrap(), 4);
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn animation_frame_hint_is_rejected_before_iteration() {
        let error = ImageDataType::decode_frames(TooManyAnimationFrames).unwrap_err();
        assert_eq!(error, "animation frame count hint exceeds limit");
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn animation_decode_keeps_frame_metadata_aligned() {
        let first = image::Frame::new(image::RgbaImage::from_raw(1, 1, vec![1, 2, 3, 4]).unwrap());
        let second = image::Frame::new(image::RgbaImage::from_raw(1, 1, vec![5, 6, 7, 8]).unwrap());
        let decoded =
            ImageDataType::decode_frames(vec![Ok(first), Ok(second)].into_iter()).unwrap();

        match decoded {
            ImageDataType::AnimRgba8 {
                width,
                height,
                frames,
                durations,
                hashes,
            } => {
                assert_eq!((width, height), (1, 1));
                assert_eq!(frames.len(), 2);
                assert_eq!(durations.len(), frames.len());
                assert_eq!(hashes.len(), frames.len());
            }
            other => panic!("expected decoded animation, got {other:?}"),
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn encoded_blob_deserialization_is_inline_and_storage_free() {
        use serde::de::value::{Error, SeqDeserializer};

        wezterm_blob_leases::clear_storage();
        let deserializer = SeqDeserializer::<_, Error>::new(vec![1_u8, 2, 3, 4].into_iter());
        let encoded = EncodedBlob::deserialize(deserializer).unwrap();

        assert_eq!(
            encoded,
            EncodedBlob::Inline(BoundedBlobBytes::new(vec![1, 2, 3, 4]).unwrap())
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn encoded_blob_deserialization_rejects_an_oversized_sequence_before_iteration() {
        use serde::de::value::{Error, SeqDeserializer};

        let oversized = std::iter::repeat(0_u8).take(MAX_WIRE_BYTE_BUFFER_BYTES + 1);
        let deserializer = SeqDeserializer::<_, Error>::new(oversized);
        let error = BoundedBlobBytes::deserialize(deserializer).unwrap_err();

        assert!(error.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn retained_size_counts_each_animation_allocation_capacity() {
        let mut first = Vec::with_capacity(11);
        first.extend_from_slice(&[1, 2, 3]);
        let mut second = Vec::with_capacity(23);
        second.extend_from_slice(&[4, 5]);
        let frames = vec![first, second];
        let durations = Vec::with_capacity(7);
        let hashes = Vec::with_capacity(5);
        let expected = core::mem::size_of::<ImageData>()
            .saturating_add(
                frames
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Vec<u8>>()),
            )
            .saturating_add(
                frames
                    .iter()
                    .fold(0usize, |size, frame| size.saturating_add(frame.capacity())),
            )
            .saturating_add(
                durations
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Duration>()),
            )
            .saturating_add(
                hashes
                    .capacity()
                    .saturating_mul(core::mem::size_of::<[u8; 32]>()),
            );
        let image = ImageData::with_data_and_hash(
            ImageDataType::AnimRgba8 {
                width: 1,
                height: 1,
                durations,
                frames,
                hashes,
            },
            [0; 32],
        );

        assert_eq!(image.retained_size(), expected);
        assert_eq!(image.len(), 5);
    }

    #[test]
    fn retention_guard_lives_until_the_final_image_owner_drops() {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let image = Arc::new(ImageData::with_data_and_hash(
            ImageDataType::Encoded(EncodedBlob::inline(Vec::new()).unwrap()),
            [0; 32],
        ));
        assert!(
            image
                .try_set_retention_guard(Guard(Arc::clone(&dropped)))
                .is_ok()
        );
        assert!(image.try_set_retention_guard(()).is_err());
        let final_owner = Arc::clone(&image);
        drop(image);
        assert!(!dropped.load(Ordering::SeqCst));
        drop(final_owner);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
