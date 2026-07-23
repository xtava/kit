//! encode and decode the frames for the mux protocol.
//! The frames include the length of a PDU as well as an identifier
//! that informs us how to decode it.  The length, ident and serial
//! number are encoded using a variable length integer encoding.
//! Rather than rely solely on serde to serialize and deserialize an
//! enum, we encode the enum variants with a version/identifier tag
//! for ourselves.  This will make it a little easier to manage
//! client and server instances that are built from different versions
//! of this code; in this way the client and server can more gracefully
//! manage unknown enum variants.
#![allow(dead_code)]
#![allow(clippy::range_plus_one)]

use anyhow::{bail, Context as _, Error};
use config::keyassignment::{PaneDirection, ScrollbackEraseMode};
use mux::client::{ClientId, ClientInfo};
use mux::pane::PaneId;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{PaneNode, SerdeUrl, SplitRequest, TabId};
use mux::window::WindowId;
use rangeset::*;
use serde::{Deserialize, Serialize};
use smol::io::AsyncWriteExt;
use smol::prelude::*;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::ffi::OsStr;
use std::io::{Cursor, Read as _};
use std::num::NonZeroU64;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::hyperlink::Hyperlink;
use termwiz::image::{ImageData, TextureCoordinate};
use termwiz::surface::{Line, SequenceNo};
use thiserror::Error;
use wezterm_runtime_admission::{
    ByteClass, BytePermit, RuntimeAdmission, MAX_DECODE_HEAP_ENVELOPE_BYTES_PER_PDU,
    MAX_DECODE_METADATA_HEAP_ENVELOPE_BYTES_PER_PDU,
    MAX_DECODE_NOTIFICATION_HEAP_ENVELOPE_BYTES_PER_PDU, MAX_DECOMPRESSED_PDU_BYTES,
    MAX_SINGLE_PDU_COMPRESSED_BYTES, MAX_SINGLE_PDU_SERIALIZED_BYTES, MAX_WIRE_BYTE_BUFFER_BYTES,
    MAX_WIRE_CONTAINERS_PER_PDU, MAX_WIRE_FRAME_BYTES, MAX_WIRE_MAP_ENTRIES_PER_PDU,
    MAX_WIRE_NESTING_DEPTH, MAX_WIRE_OWNED_PAYLOAD_BYTES_PER_PDU, MAX_WIRE_SEQUENCE_ITEMS_PER_PDU,
    MAX_WIRE_STRING_BYTES,
};
use wezterm_term::color::ColorPalette;
use wezterm_term::{Alert, ClipboardSelection, StableRowIndex, TerminalSize};
use zeroize::Zeroize;

#[derive(Error, Debug)]
#[error("Corrupt Response: {0}")]
pub struct CorruptResponse(String);

/// Returns the encoded length of the leb128 representation of value
fn encoded_length(value: u64) -> usize {
    struct NullWrite {}
    impl std::io::Write for NullWrite {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, std::io::Error> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
            Ok(())
        }
    }

    leb128::write::unsigned(&mut NullWrite {}, value).unwrap()
}

const COMPRESSED_MASK: u64 = 1 << 63;

struct LimitedWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
}

impl<W> LimitedWriter<W> {
    fn new(inner: W, limit: usize) -> Self {
        Self {
            inner,
            written: 0,
            limit,
        }
    }
}

impl<W: std::io::Write> std::io::Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let next = self
            .written
            .checked_add(buf.len())
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "serialized PDU exceeds its finite envelope",
                )
            })?;
        self.inner.write_all(buf)?;
        self.written = next;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn encode_raw_as_vec(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
) -> anyhow::Result<Vec<u8>> {
    let len = data
        .len()
        .checked_add(encoded_length(ident))
        .and_then(|len| len.checked_add(encoded_length(serial)))
        .ok_or_else(|| CorruptResponse("encoded PDU length overflow".to_string()))?;
    let masked_len = if is_compressed {
        (len as u64) | COMPRESSED_MASK
    } else {
        len as u64
    };

    // Double-buffer the data; since we run with nodelay enabled, it is
    // desirable for the write to be a single packet (or at least, for
    // the header portion to go out in a single packet)
    let frame_len = len
        .checked_add(encoded_length(masked_len))
        .ok_or_else(|| CorruptResponse("encoded frame length overflow".to_string()))?;
    if frame_len > MAX_WIRE_FRAME_BYTES {
        bail!("encoded PDU exceeds the wire frame limit");
    }
    let mut buffer = Vec::with_capacity(frame_len);

    leb128::write::unsigned(&mut buffer, masked_len).context("writing pdu len")?;
    leb128::write::unsigned(&mut buffer, serial).context("writing pdu serial")?;
    leb128::write::unsigned(&mut buffer, ident).context("writing pdu ident")?;
    buffer.extend_from_slice(data);

    if is_compressed {
        metrics::histogram!("pdu.encode.compressed.size").record(buffer.len() as f64);
    } else {
        metrics::histogram!("pdu.encode.size").record(buffer.len() as f64);
    }

    Ok(buffer)
}

/// Encode a frame.  If the data is compressed, the high bit of the length
/// is set to indicate that.  The data written out has the format:
/// tagged_len: leb128  (u64 msb is set if data is compressed)
/// serial: leb128
/// ident: leb128
/// data bytes
fn encode_raw<W: std::io::Write>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    mut w: W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer).context("writing pdu data buffer")?;
    Ok(buffer.len())
}

async fn encode_raw_async<W: Unpin + AsyncWriteExt>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    w: &mut W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer)
        .await
        .context("writing pdu data buffer")?;
    Ok(buffer.len())
}

#[derive(Clone, Copy, Debug)]
struct EncodedU64 {
    value: u64,
    encoded_len: usize,
}

/// Read a single leb128 encoded value from the stream.
async fn read_u64_async<R>(r: &mut R, max_bytes: usize) -> anyhow::Result<EncodedU64>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
{
    let mut buf = vec![];
    loop {
        if buf.len() >= max_bytes {
            bail!("leb128 value extends beyond its admitted header field");
        }
        let mut byte = [0u8];
        let nread = r.read(&mut byte).await?;
        if nread == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF while reading leb128 encoded value",
            )
            .into());
        }
        buf.push(byte[0]);

        match leb128::read::unsigned(&mut buf.as_slice()) {
            Ok(n) => {
                return Ok(EncodedU64 {
                    value: n,
                    encoded_len: buf.len(),
                });
            }
            Err(leb128::read::Error::IoError(_)) => continue,
            Err(leb128::read::Error::Overflow) => anyhow::bail!("leb128 is too large"),
        }
    }
}

/// Read a single leb128 encoded value from the stream.
fn read_u64<R: std::io::Read>(r: &mut R, max_bytes: usize) -> anyhow::Result<EncodedU64> {
    let mut buf = Vec::new();
    loop {
        if buf.len() >= max_bytes {
            bail!("leb128 value extends beyond its admitted header field");
        }
        let mut byte = [0u8];
        r.read_exact(&mut byte).context("reading leb128")?;
        buf.push(byte[0]);
        match leb128::read::unsigned(&mut buf.as_slice()) {
            Ok(value) => {
                return Ok(EncodedU64 {
                    value,
                    encoded_len: buf.len(),
                });
            }
            Err(leb128::read::Error::IoError(_)) => continue,
            Err(leb128::read::Error::Overflow) => bail!("leb128 is too large"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeDirection {
    ClientToServer,
    ServerToClient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRequestPhase {
    Bootstrap,
    Established,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodeCorrelation {
    Request { phase: ClientRequestPhase },
    Notification,
    Response { expected: Option<PduTag> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeContext {
    direction: DecodeDirection,
    correlation: DecodeCorrelation,
}

impl DecodeContext {
    pub fn client_to_server_request(phase: ClientRequestPhase) -> Self {
        Self {
            direction: DecodeDirection::ClientToServer,
            correlation: DecodeCorrelation::Request { phase },
        }
    }

    pub fn server_to_client_notification() -> Self {
        Self {
            direction: DecodeDirection::ServerToClient,
            correlation: DecodeCorrelation::Notification,
        }
    }

    pub fn server_to_client_response(expected: Option<PduTag>) -> Self {
        Self {
            direction: DecodeDirection::ServerToClient,
            correlation: DecodeCorrelation::Response { expected },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PduHeaderPolicy {
    Request { response: PduTag },
    Response,
    Notification,
    RequestOrNotification { response: PduTag },
}

impl PduHeaderPolicy {
    fn allows_notification(self) -> bool {
        matches!(
            self,
            Self::Notification | Self::RequestOrNotification { .. }
        )
    }

    fn expected_response(self) -> Option<PduTag> {
        match self {
            Self::Request { response } | Self::RequestOrNotification { response } => Some(response),
            Self::Response | Self::Notification => None,
        }
    }
}

#[derive(Debug)]
pub struct StagedPduHeader {
    serial: u64,
    tag: PduTag,
    body_len: usize,
    is_compressed: bool,
}

impl StagedPduHeader {
    pub fn serial(&self) -> u64 {
        self.serial
    }

    pub fn tag(&self) -> PduTag {
        self.tag
    }

    pub fn validate(
        self,
        context: DecodeContext,
        admission: &RuntimeAdmission,
    ) -> anyhow::Result<AdmittedPduBody> {
        let policy = self.tag.header_policy();
        match (context.direction, context.correlation) {
            (DecodeDirection::ClientToServer, DecodeCorrelation::Request { phase }) => {
                if self.serial == 0 {
                    bail!(
                        "client request PDU {} has notification serial zero",
                        self.tag.name()
                    );
                }
                if !self.tag.allows_client_request_phase(phase) {
                    bail!(
                        "PDU {} is invalid for client request phase {:?}",
                        self.tag.name(),
                        phase
                    );
                }
            }
            (DecodeDirection::ServerToClient, DecodeCorrelation::Notification) => {
                if self.serial != 0 {
                    bail!(
                        "server notification PDU {} has correlated serial {}",
                        self.tag.name(),
                        self.serial
                    );
                }
                if !policy.allows_notification() {
                    bail!(
                        "PDU {} is invalid as a server notification",
                        self.tag.name()
                    );
                }
            }
            (DecodeDirection::ServerToClient, DecodeCorrelation::Response { expected }) => {
                if self.serial == 0 {
                    bail!(
                        "correlated response PDU {} has serial zero",
                        self.tag.name()
                    );
                }
                let expected = expected.ok_or_else(|| {
                    CorruptResponse(format!(
                        "response serial {} has no corresponding promise",
                        self.serial
                    ))
                })?;
                if self.tag != expected && self.tag != PduTag::ErrorResponse {
                    bail!(
                        "response serial {} expected {} but received {}",
                        self.serial,
                        expected.name(),
                        self.tag.name()
                    );
                }
            }
            _ => {
                unreachable!("DecodeContext constructors define valid direction/correlation pairs")
            }
        }
        let wire = admission
            .try_bytes(ByteClass::DecodeWorking, self.body_len)
            .context("wire decode admission")?;
        Ok(AdmittedPduBody {
            serial: self.serial,
            tag: self.tag,
            body_len: self.body_len,
            is_compressed: self.is_compressed,
            wire,
        })
    }
}

#[derive(Debug)]
pub struct AdmittedPduBody {
    serial: u64,
    tag: PduTag,
    body_len: usize,
    is_compressed: bool,
    wire: BytePermit,
}

#[derive(Clone, Copy, Debug)]
struct FramePrefix {
    content_len: usize,
    is_compressed: bool,
}

fn parse_frame_prefix(tagged_len: EncodedU64) -> anyhow::Result<FramePrefix> {
    let (content_len, is_compressed) = if (tagged_len.value & COMPRESSED_MASK) != 0 {
        (tagged_len.value & !COMPRESSED_MASK, true)
    } else {
        (tagged_len.value, false)
    };
    let content_len = usize::try_from(content_len)
        .map_err(|_| CorruptResponse("wire frame length does not fit usize".to_string()))?;
    let frame_len = tagged_len
        .encoded_len
        .checked_add(content_len)
        .ok_or_else(|| CorruptResponse("wire frame length overflow".to_string()))?;
    if frame_len > MAX_WIRE_FRAME_BYTES {
        return Err(CorruptResponse("wire frame exceeds the finite envelope".to_string()).into());
    }
    if content_len < 2 {
        return Err(CorruptResponse(
            "wire content cannot contain both serial and identifier".to_string(),
        )
        .into());
    }
    Ok(FramePrefix {
        content_len,
        is_compressed,
    })
}

fn finish_header(
    prefix: FramePrefix,
    serial: EncodedU64,
    ident: EncodedU64,
) -> anyhow::Result<StagedPduHeader> {
    let tag = PduTag::from_ident(ident.value)
        .ok_or_else(|| CorruptResponse(format!("unknown PDU identifier {}", ident.value)))?;
    let header_len = serial
        .encoded_len
        .checked_add(ident.encoded_len)
        .ok_or_else(|| CorruptResponse("wire header length overflow".to_string()))?;
    let body_len = prefix.content_len.checked_sub(header_len).ok_or_else(|| {
        CorruptResponse(format!(
            "wire content length {} is smaller than its serial and identifier",
            prefix.content_len
        ))
    })?;
    Ok(StagedPduHeader {
        serial: serial.value,
        tag,
        body_len,
        is_compressed: prefix.is_compressed,
    })
}

async fn read_header_async<R>(r: &mut R) -> anyhow::Result<StagedPduHeader>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
{
    let tagged_len = read_u64_async(r, 10)
        .await
        .context("reading async PDU length")?;
    let prefix = parse_frame_prefix(tagged_len)?;
    let serial = read_u64_async(r, prefix.content_len - 1)
        .await
        .context("reading async PDU serial")?;
    let ident = read_u64_async(r, prefix.content_len - serial.encoded_len)
        .await
        .context("reading async PDU identifier")?;
    finish_header(prefix, serial, ident)
}

fn read_header<R: std::io::Read>(r: &mut R) -> anyhow::Result<StagedPduHeader> {
    let tagged_len = read_u64(r, 10).context("reading PDU length")?;
    let prefix = parse_frame_prefix(tagged_len)?;
    let serial = read_u64(r, prefix.content_len - 1).context("reading PDU serial")?;
    let ident =
        read_u64(r, prefix.content_len - serial.encoded_len).context("reading PDU identifier")?;
    finish_header(prefix, serial, ident)
}

async fn read_body_async<R>(r: &mut R, body: &AdmittedPduBody) -> anyhow::Result<Vec<u8>>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
{
    let mut data = Vec::new();
    data.try_reserve_exact(body.body_len)
        .context("reserving bounded wire frame")?;
    data.resize(body.body_len, 0);
    r.read_exact(&mut data).await.with_context(|| {
        format!(
            "reading {} async body bytes for serial={} tag={}",
            body.body_len,
            body.serial,
            body.tag.name()
        )
    })?;
    Ok(data)
}

fn read_body<R: std::io::Read>(r: &mut R, body: &AdmittedPduBody) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    data.try_reserve_exact(body.body_len)
        .context("reserving bounded wire frame")?;
    data.resize(body.body_len, 0);
    r.read_exact(&mut data).with_context(|| {
        format!(
            "reading {} body bytes for serial={} tag={}",
            body.body_len,
            body.serial,
            body.tag.name()
        )
    })?;
    Ok(data)
}

/// A successfully decoded inbound PDU whose decoded-heap reservation remains live.
///
/// Outbound PDUs are represented directly by `Pdu` plus their serial. Keeping this
/// type inbound-only makes it impossible to construct decoded data without the
/// reservation that bounded its retained heap materialization. The decoder releases
/// the separate wire-buffer permit as soon as the source buffer is dropped.
#[derive(Debug)]
pub struct AdmittedDecodedPdu {
    serial: u64,
    pdu: Pdu,
    reservation: DecodeReservation,
}

#[derive(Debug)]
pub struct DecodeReservation {
    _heap: BytePermit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationSerial(());

impl NotificationSerial {
    pub const fn get(self) -> u64 {
        0
    }
}

#[derive(Debug)]
pub struct AdmittedNotification {
    serial: NotificationSerial,
    pdu: Pdu,
    reservation: DecodeReservation,
}

impl AdmittedNotification {
    pub fn serial(&self) -> NotificationSerial {
        self.serial
    }

    pub fn pdu(&self) -> &Pdu {
        &self.pdu
    }

    pub fn into_parts(self) -> (NotificationSerial, Pdu, DecodeReservation) {
        (self.serial, self.pdu, self.reservation)
    }
}

#[derive(Debug)]
pub struct AdmittedRpcResponse<T> {
    serial: NonZeroU64,
    value: T,
    reservation: DecodeReservation,
}

impl<T> AdmittedRpcResponse<T> {
    pub fn serial(&self) -> u64 {
        self.serial.get()
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn into_parts(self) -> (u64, T, DecodeReservation) {
        (self.serial.get(), self.value, self.reservation)
    }

    pub fn into_inner(self) -> T {
        self.value
    }

    pub fn try_map<U, E>(
        self,
        map: impl FnOnce(T) -> Result<U, E>,
    ) -> Result<AdmittedRpcResponse<U>, E> {
        let Self {
            serial,
            value,
            reservation,
        } = self;
        Ok(AdmittedRpcResponse {
            serial,
            value: map(value)?,
            reservation,
        })
    }
}

impl PartialEq for AdmittedDecodedPdu {
    fn eq(&self, other: &Self) -> bool {
        self.serial == other.serial && self.pdu == other.pdu
    }
}

impl AdmittedDecodedPdu {
    pub fn serial(&self) -> u64 {
        self.serial
    }

    pub fn pdu(&self) -> &Pdu {
        &self.pdu
    }

    pub fn into_parts(self) -> (u64, Pdu, DecodeReservation) {
        (self.serial, self.pdu, self.reservation)
    }

    pub fn into_notification(self) -> Result<AdmittedNotification, Error> {
        let (serial, pdu, reservation) = self.into_parts();
        if serial != 0 {
            bail!("correlated PDU serial {serial} cannot become a notification");
        }
        Ok(AdmittedNotification {
            serial: NotificationSerial(()),
            pdu,
            reservation,
        })
    }

    pub fn into_rpc_response(self) -> Result<AdmittedRpcResponse<Pdu>, Error> {
        let (serial, pdu, reservation) = self.into_parts();
        let serial = NonZeroU64::new(serial)
            .ok_or_else(|| CorruptResponse("notification cannot become an RPC response".into()))?;
        Ok(AdmittedRpcResponse {
            serial,
            value: pdu,
            reservation,
        })
    }
}

/// If the serialized size is larger than this, then we'll consider compressing it
const COMPRESS_THRESH: usize = 32;

fn serialize<T: serde::Serialize>(
    t: &T,
    admission: &RuntimeAdmission,
) -> Result<(Vec<u8>, bool, BytePermit), Error> {
    let permit = admission.try_bytes(
        ByteClass::EncodeWorking,
        MAX_SINGLE_PDU_SERIALIZED_BYTES + MAX_SINGLE_PDU_COMPRESSED_BYTES,
    )?;
    let mut uncompressed = Vec::new();
    let mut bounded = LimitedWriter::new(&mut uncompressed, MAX_SINGLE_PDU_SERIALIZED_BYTES);
    let mut encode = varbincode::Serializer::new(&mut bounded);
    t.serialize(&mut encode)?;

    if uncompressed.len() <= COMPRESS_THRESH {
        return Ok((uncompressed, false, permit));
    }
    // It's a little heavy; let's try compressing it
    let mut compressed = Vec::new();
    let bounded = LimitedWriter::new(&mut compressed, MAX_SINGLE_PDU_COMPRESSED_BYTES);
    let mut compress = zstd::Encoder::new(bounded, zstd::DEFAULT_COMPRESSION_LEVEL)?;
    {
        let mut encode = varbincode::Serializer::new(&mut compress);
        t.serialize(&mut encode)?;
    }
    compress.finish()?;

    log::debug!(
        "serialized+compress len {} vs {}",
        compressed.len(),
        uncompressed.len()
    );

    if compressed.len() < uncompressed.len() {
        Ok((compressed, true, permit))
    } else {
        Ok((uncompressed, false, permit))
    }
}

fn deserialize<T: serde::de::DeserializeOwned, R: std::io::Read>(
    mut r: R,
    is_compressed: bool,
) -> Result<T, Error> {
    let limits = varbincode::DecodeLimits {
        max_owned_payload_bytes: MAX_WIRE_OWNED_PAYLOAD_BYTES_PER_PDU,
        max_string_bytes: MAX_WIRE_STRING_BYTES,
        max_byte_buffer_bytes: MAX_WIRE_BYTE_BUFFER_BYTES,
        max_sequence_items: MAX_WIRE_SEQUENCE_ITEMS_PER_PDU,
        max_map_entries: MAX_WIRE_MAP_ENTRIES_PER_PDU,
        max_containers: MAX_WIRE_CONTAINERS_PER_PDU,
        max_nesting_depth: MAX_WIRE_NESTING_DEPTH,
    };
    if is_compressed {
        let mut decompress = zstd::Decoder::new(r)?;
        let mut data = Vec::new();
        std::io::Read::take(&mut decompress, (MAX_DECOMPRESSED_PDU_BYTES + 1) as u64)
            .read_to_end(&mut data)?;
        if data.len() > MAX_DECOMPRESSED_PDU_BYTES {
            bail!("decompressed PDU exceeds the finite envelope");
        }
        let mut data = data.as_slice();
        let value = {
            let mut decode = varbincode::Deserializer::new(&mut data, limits);
            serde::Deserialize::deserialize(&mut decode)?
        };
        if !data.is_empty() {
            bail!("trailing bytes after compressed PDU body");
        }
        Ok(value)
    } else {
        let value = {
            let mut decode = varbincode::Deserializer::new(&mut r, limits);
            serde::Deserialize::deserialize(&mut decode)?
        };
        let mut trailing = [0u8; 1];
        if r.read(&mut trailing)? != 0 {
            bail!("trailing bytes after PDU body");
        }
        Ok(value)
    }
}

macro_rules! pdu_header_policy {
    (Request($response:ident)) => {
        PduHeaderPolicy::Request {
            response: PduTag::$response,
        }
    };
    (Response) => {
        PduHeaderPolicy::Response
    };
    (Notification) => {
        PduHeaderPolicy::Notification
    };
    (RequestOrNotification($response:ident)) => {
        PduHeaderPolicy::RequestOrNotification {
            response: PduTag::$response,
        }
    };
}

macro_rules! pdu {
    ($( $name:ident:$vers:expr => $policy:ident $(($response:ident))?),* $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum PduTag {
            $(
                $name,
            )*
        }

        impl PduTag {
            pub const ALL: &'static [Self] = &[$(Self::$name,)*];

            fn from_ident(ident: u64) -> Option<Self> {
                match ident {
                    $( $vers => Some(Self::$name), )*
                    _ => None,
                }
            }

            pub fn ident(self) -> u64 {
                match self {
                    $( Self::$name => $vers, )*
                }
            }

            pub fn name(self) -> &'static str {
                match self {
                    $( Self::$name => stringify!($name), )*
                }
            }

            pub fn header_policy(self) -> PduHeaderPolicy {
                match self {
                    $( Self::$name => pdu_header_policy!($policy $(($response))?), )*
                }
            }

            pub fn expected_response(self) -> Option<Self> {
                self.header_policy().expected_response()
            }
        }

        #[derive(PartialEq, Debug)]
        pub enum Pdu {
            Invalid{ident: u64},
            $(
                $name($name)
            ,)*
        }

        impl Pdu {
            pub fn encode<W: std::io::Write>(
                &self,
                w: W,
                serial: u64,
                admission: &RuntimeAdmission,
            ) -> Result<(), Error> {
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed, _permit) = serialize(s, admission)?;
                            let encoded_size = encode_raw($vers, serial, &data, is_compressed, w)?;
                            log::debug!("encode {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            pub async fn encode_async<W: Unpin + AsyncWriteExt>(
                &self,
                w: &mut W,
                serial: u64,
                admission: &RuntimeAdmission,
            ) -> Result<(), Error> {
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed, _permit) = serialize(s, admission)?;
                            let encoded_size = encode_raw_async($vers, serial, &data, is_compressed, w).await?;
                            log::debug!("encode_async {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            pub fn pdu_name(&self) -> &'static str {
                match self {
                    Pdu::Invalid{..} => "Invalid",
                    $(
                        Pdu::$name(_) => {
                            stringify!($name)
                        }
                    ,)*
                }
            }

            pub fn tag(&self) -> Option<PduTag> {
                match self {
                    Pdu::Invalid { .. } => None,
                    $(
                        Pdu::$name(_) => Some(PduTag::$name),
                    )*
                }
            }

            pub fn expected_response_tag(&self) -> Option<PduTag> {
                self.tag().and_then(PduTag::expected_response)
            }

            pub fn read_header<R: std::io::Read>(
                r: &mut R,
            ) -> Result<StagedPduHeader, Error> {
                read_header(r)
            }

            pub async fn read_header_async<R>(
                r: &mut R,
            ) -> Result<StagedPduHeader, Error>
                where R: std::marker::Unpin,
                      R: AsyncRead,
                      R: std::fmt::Debug
            {
                read_header_async(r).await
            }

            pub fn decode<R: std::io::Read>(
                mut r: R,
                context: DecodeContext,
                admission: &RuntimeAdmission,
            ) -> Result<AdmittedDecodedPdu, Error> {
                let header = Self::read_header(&mut r)
                    .context("reading a PDU header")?;
                let body = header
                    .validate(context, admission)
                    .context("validating a PDU header")?;
                Self::decode_body(&mut r, body, admission)
            }

            fn decode_body<R: std::io::Read>(
                r: &mut R,
                body: AdmittedPduBody,
                admission: &RuntimeAdmission,
            ) -> Result<AdmittedDecodedPdu, Error> {
                let data = read_body(r, &body).context("reading a PDU body")?;
                Self::decode_admitted(body, data, admission)
            }

            pub async fn decode_body_async<R>(
                r: &mut R,
                body: AdmittedPduBody,
                admission: &RuntimeAdmission,
            ) -> Result<AdmittedDecodedPdu, Error>
                where R: std::marker::Unpin,
                      R: AsyncRead,
                      R: std::fmt::Debug
            {
                let data = read_body_async(r, &body)
                    .await
                    .context("reading an async PDU body")?;
                Self::decode_admitted(body, data, admission)
            }

            fn decode_admitted(
                body: AdmittedPduBody,
                data: Vec<u8>,
                admission: &RuntimeAdmission,
            ) -> Result<AdmittedDecodedPdu, Error> {
                let heap_envelope = body.tag.decode_heap_envelope();
                let heap = admission
                    .try_bytes(ByteClass::DecodeWorking, heap_envelope)
                    .with_context(|| {
                        format!("reserving decoded heap envelope for {}", body.tag.name())
                    })?;
                let AdmittedPduBody { serial, tag, is_compressed, wire, .. } = body;
                let pdu = match tag {
                    $(
                        PduTag::$name => {
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(data.len() as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(data.len() as f64);
                            Pdu::$name(deserialize(data.as_slice(), is_compressed)?)
                        }
                    ,)*
                };
                drop(data);
                drop(wire);
                Ok(AdmittedDecodedPdu {
                    serial,
                    pdu,
                    reservation: DecodeReservation { _heap: heap },
                })
            }
        }
    }
}

/// The overall version of the codec.
/// This must be bumped when backwards incompatible changes
/// are made to the types and protocol.
pub const CODEC_VERSION: usize = 52;

// Defines the Pdu enum.
// Each struct has an explicit identifying number.
// This allows removal of obsolete structs,
// and defining newer structs as the protocol evolves.
pdu! {
    ErrorResponse: 0 => Response,
    Ping: 1 => Request(Pong),
    Pong: 2 => Response,
    ListPanes: 3 => Request(ListPanesResponse),
    ListPanesResponse: 4 => Response,
    SpawnResponse: 8 => Response,
    WriteToPane: 9 => Request(UnitResponse),
    UnitResponse: 10 => Response,
    SendKeyDown: 11 => Request(UnitResponse),
    SendMouseEvent: 12 => Request(UnitResponse),
    SendPaste: 13 => Request(UnitResponse),
    Resize: 14 => Request(UnitResponse),
    SetClipboard: 20 => Notification,
    GetLines: 22 => Request(GetLinesResponse),
    GetLinesResponse: 23 => Response,
    GetPaneRenderChanges: 24 => Request(LivenessResponse),
    GetPaneRenderChangesResponse: 25 => Notification,
    GetCodecVersion: 26 => Request(GetCodecVersionResponse),
    GetCodecVersionResponse: 27 => Response,
    GetTlsCreds: 28 => Request(GetTlsCredsResponse),
    GetTlsCredsResponse: 29 => Response,
    LivenessResponse: 30 => Response,
    SearchScrollbackRequest: 31 => Request(SearchScrollbackResponse),
    SearchScrollbackResponse: 32 => Response,
    SetPaneZoomed: 33 => Request(UnitResponse),
    SplitPane: 34 => Request(SpawnResponse),
    KillPane: 35 => Request(UnitResponse),
    SpawnV2: 36 => Request(SpawnResponse),
    PaneRemoved: 37 => Notification,
    SetPalette: 38 => RequestOrNotification(UnitResponse),
    NotifyAlert: 39 => Notification,
    SetClientId: 40 => Request(SetClientIdResponse),
    GetClientList: 41 => Request(GetClientListResponse),
    GetClientListResponse: 42 => Response,
    SetWindowWorkspace: 43 => Request(UnitResponse),
    WindowWorkspaceChanged: 44 => Notification,
    SetFocusedPane: 45 => Request(UnitResponse),
    GetImageCell: 46 => Request(GetImageCellResponse),
    GetImageCellResponse: 47 => Response,
    MovePaneToNewTab: 48 => Request(MovePaneToNewTabResponse),
    MovePaneToNewTabResponse: 49 => Response,
    ActivatePaneDirection: 50 => Request(UnitResponse),
    GetPaneRenderableDimensions: 51 => Request(GetPaneRenderableDimensionsResponse),
    GetPaneRenderableDimensionsResponse: 52 => Response,
    PaneFocused: 53 => Notification,
    TabResized: 54 => Notification,
    TabAddedToWindow: 55 => Notification,
    TabTitleChanged: 56 => RequestOrNotification(UnitResponse),
    WindowTitleChanged: 57 => RequestOrNotification(UnitResponse),
    RenameWorkspace: 58 => RequestOrNotification(UnitResponse),
    EraseScrollbackRequest: 59 => Request(UnitResponse),
    GetPaneDirection: 60 => Request(GetPaneDirectionResponse),
    GetPaneDirectionResponse: 61 => Response,
    AdjustPaneSize: 62 => Request(UnitResponse),
    GetBuildIdentity: 63 => Request(GetBuildIdentityResponse),
    GetBuildIdentityResponse: 64 => Response,
    ControlLeaseRequest: 65 => Request(ControlLeaseResult),
    ControlLeaseResult: 66 => Response,
    ControlSnapshot: 67 => Notification,
    ControlChanged: 68 => Notification,
    AttachRejected: 69 => Notification,
    ServiceDrainRequest: 70 => Request(ServiceDrainResult),
    ServiceDrainResult: 71 => Response,
    SetClientIdResponse: 72 => Response,
}

impl PduTag {
    fn decode_heap_envelope(self) -> usize {
        match self {
            Self::UnitResponse | Self::Pong | Self::AttachRejected => 0,
            Self::SpawnResponse
            | Self::GetPaneRenderableDimensionsResponse
            | Self::LivenessResponse
            | Self::GetPaneDirectionResponse
            | Self::PaneRemoved
            | Self::PaneFocused
            | Self::TabResized
            | Self::TabAddedToWindow
            | Self::ControlLeaseResult
            | Self::ControlSnapshot
            | Self::ControlChanged
            | Self::SetClientIdResponse => MAX_DECODE_METADATA_HEAP_ENVELOPE_BYTES_PER_PDU,
            Self::ServiceDrainResult => MAX_DECODE_METADATA_HEAP_ENVELOPE_BYTES_PER_PDU,
            Self::SetPalette
            | Self::NotifyAlert
            | Self::TabTitleChanged
            | Self::WindowTitleChanged
            | Self::WindowWorkspaceChanged => MAX_DECODE_NOTIFICATION_HEAP_ENVELOPE_BYTES_PER_PDU,
            _ => MAX_DECODE_HEAP_ENVELOPE_BYTES_PER_PDU,
        }
    }

    fn allows_client_request_phase(self, phase: ClientRequestPhase) -> bool {
        match self {
            Self::Ping | Self::GetCodecVersion | Self::GetTlsCreds | Self::GetBuildIdentity => true,
            Self::SetClientId => phase == ClientRequestPhase::Bootstrap,
            Self::ListPanes
            | Self::WriteToPane
            | Self::SendKeyDown
            | Self::SendMouseEvent
            | Self::SendPaste
            | Self::Resize
            | Self::GetLines
            | Self::GetPaneRenderChanges
            | Self::SearchScrollbackRequest
            | Self::SetPaneZoomed
            | Self::SplitPane
            | Self::KillPane
            | Self::SpawnV2
            | Self::SetPalette
            | Self::GetClientList
            | Self::SetWindowWorkspace
            | Self::SetFocusedPane
            | Self::GetImageCell
            | Self::MovePaneToNewTab
            | Self::ActivatePaneDirection
            | Self::GetPaneRenderableDimensions
            | Self::TabTitleChanged
            | Self::WindowTitleChanged
            | Self::RenameWorkspace
            | Self::EraseScrollbackRequest
            | Self::GetPaneDirection
            | Self::AdjustPaneSize
            | Self::ControlLeaseRequest
            | Self::ServiceDrainRequest => phase == ClientRequestPhase::Established,
            Self::ErrorResponse
            | Self::Pong
            | Self::ListPanesResponse
            | Self::SpawnResponse
            | Self::UnitResponse
            | Self::SetClipboard
            | Self::GetLinesResponse
            | Self::GetPaneRenderChangesResponse
            | Self::GetCodecVersionResponse
            | Self::GetTlsCredsResponse
            | Self::LivenessResponse
            | Self::SearchScrollbackResponse
            | Self::PaneRemoved
            | Self::NotifyAlert
            | Self::GetClientListResponse
            | Self::WindowWorkspaceChanged
            | Self::GetImageCellResponse
            | Self::MovePaneToNewTabResponse
            | Self::GetPaneRenderableDimensionsResponse
            | Self::PaneFocused
            | Self::TabResized
            | Self::TabAddedToWindow
            | Self::GetPaneDirectionResponse
            | Self::GetBuildIdentityResponse
            | Self::ControlLeaseResult
            | Self::ControlSnapshot
            | Self::ControlChanged
            | Self::AttachRejected
            | Self::ServiceDrainResult
            | Self::SetClientIdResponse => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestOperation {
    Ping,
    ListPanes,
    Spawn,
    WriteToPane,
    SendKey,
    SendMouse,
    SendPaste,
    Resize,
    SetZoom,
    GetLines,
    GetRenderChanges,
    GetCodecVersion,
    GetBuildIdentity,
    GetTlsCredentials,
    SearchScrollback,
    SplitPane,
    KillPane,
    RegisterClient,
    GetClientList,
    SetWindowWorkspace,
    SetFocusedPane,
    GetImageCell,
    MovePaneToNewTab,
    ActivatePaneDirection,
    GetRenderableDimensions,
    SetPalette,
    SetTabTitle,
    SetWindowTitle,
    RenameWorkspace,
    EraseScrollback,
    GetPaneDirection,
    AdjustPaneSize,
    ControlLease,
    ServiceDrain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestAuthority {
    Bootstrap,
    Observe,
    PaneControl(PaneControlTargets),
    ControlLease(PaneId),
    UntargetedMutation,
    HostSensitive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaneControlTargets {
    pub primary: PaneId,
    pub secondary: Option<PaneId>,
}

impl PaneControlTargets {
    fn one(primary: PaneId) -> Self {
        Self {
            primary,
            secondary: None,
        }
    }
}

#[derive(Debug, Error)]
#[error("PDU {pdu} is invalid in the client-to-server direction")]
pub struct InvalidPduDirection {
    pub pdu: &'static str,
}

impl Pdu {
    pub fn request_operation(&self) -> Result<RequestOperation, InvalidPduDirection> {
        let operation = match self {
            Self::Ping(_) => RequestOperation::Ping,
            Self::ListPanes(_) => RequestOperation::ListPanes,
            Self::SpawnV2(_) => RequestOperation::Spawn,
            Self::WriteToPane(_) => RequestOperation::WriteToPane,
            Self::SendKeyDown(_) => RequestOperation::SendKey,
            Self::SendMouseEvent(_) => RequestOperation::SendMouse,
            Self::SendPaste(_) => RequestOperation::SendPaste,
            Self::Resize(_) => RequestOperation::Resize,
            Self::SetPaneZoomed(_) => RequestOperation::SetZoom,
            Self::GetLines(_) => RequestOperation::GetLines,
            Self::GetPaneRenderChanges(_) => RequestOperation::GetRenderChanges,
            Self::GetCodecVersion(_) => RequestOperation::GetCodecVersion,
            Self::GetBuildIdentity(_) => RequestOperation::GetBuildIdentity,
            Self::GetTlsCreds(_) => RequestOperation::GetTlsCredentials,
            Self::SearchScrollbackRequest(_) => RequestOperation::SearchScrollback,
            Self::SplitPane(_) => RequestOperation::SplitPane,
            Self::KillPane(_) => RequestOperation::KillPane,
            Self::SetClientId(_) => RequestOperation::RegisterClient,
            Self::GetClientList(_) => RequestOperation::GetClientList,
            Self::SetWindowWorkspace(_) => RequestOperation::SetWindowWorkspace,
            Self::SetFocusedPane(_) => RequestOperation::SetFocusedPane,
            Self::GetImageCell(_) => RequestOperation::GetImageCell,
            Self::MovePaneToNewTab(_) => RequestOperation::MovePaneToNewTab,
            Self::ActivatePaneDirection(_) => RequestOperation::ActivatePaneDirection,
            Self::GetPaneRenderableDimensions(_) => RequestOperation::GetRenderableDimensions,
            Self::SetPalette(_) => RequestOperation::SetPalette,
            Self::TabTitleChanged(_) => RequestOperation::SetTabTitle,
            Self::WindowTitleChanged(_) => RequestOperation::SetWindowTitle,
            Self::RenameWorkspace(_) => RequestOperation::RenameWorkspace,
            Self::EraseScrollbackRequest(_) => RequestOperation::EraseScrollback,
            Self::GetPaneDirection(_) => RequestOperation::GetPaneDirection,
            Self::AdjustPaneSize(_) => RequestOperation::AdjustPaneSize,
            Self::ControlLeaseRequest(_) => RequestOperation::ControlLease,
            Self::ServiceDrainRequest(_) => RequestOperation::ServiceDrain,
            Self::Invalid { .. }
            | Self::ErrorResponse(_)
            | Self::Pong(_)
            | Self::ListPanesResponse(_)
            | Self::SpawnResponse(_)
            | Self::UnitResponse(_)
            | Self::SetClipboard(_)
            | Self::GetLinesResponse(_)
            | Self::GetPaneRenderChangesResponse(_)
            | Self::GetCodecVersionResponse(_)
            | Self::GetBuildIdentityResponse(_)
            | Self::GetTlsCredsResponse(_)
            | Self::LivenessResponse(_)
            | Self::SearchScrollbackResponse(_)
            | Self::PaneRemoved(_)
            | Self::NotifyAlert(_)
            | Self::WindowWorkspaceChanged(_)
            | Self::GetClientListResponse(_)
            | Self::GetImageCellResponse(_)
            | Self::MovePaneToNewTabResponse(_)
            | Self::GetPaneRenderableDimensionsResponse(_)
            | Self::PaneFocused(_)
            | Self::TabResized(_)
            | Self::TabAddedToWindow(_)
            | Self::GetPaneDirectionResponse(_)
            | Self::ControlLeaseResult(_)
            | Self::ServiceDrainResult(_)
            | Self::SetClientIdResponse(_)
            | Self::ControlSnapshot(_)
            | Self::ControlChanged(_)
            | Self::AttachRejected(_) => {
                return Err(InvalidPduDirection {
                    pdu: self.pdu_name(),
                })
            }
        };
        Ok(operation)
    }

    /// Exhaustive server-side authority required before dispatching an inbound request.
    pub fn request_authority(&self) -> Result<RequestAuthority, InvalidPduDirection> {
        let authority = match self {
            Self::Ping(_)
            | Self::GetCodecVersion(_)
            | Self::GetBuildIdentity(_)
            | Self::SetClientId(_) => RequestAuthority::Bootstrap,
            Self::ListPanes(_)
            | Self::GetLines(_)
            | Self::GetPaneRenderChanges(_)
            | Self::SearchScrollbackRequest(_)
            | Self::GetImageCell(_)
            | Self::GetPaneRenderableDimensions(_)
            | Self::GetPaneDirection(_) => RequestAuthority::Observe,
            Self::WriteToPane(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::SendKeyDown(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::SendMouseEvent(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::SendPaste(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::Resize(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::SetPaneZoomed(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::SplitPane(request) => RequestAuthority::PaneControl(PaneControlTargets {
                primary: request.target_pane_id,
                secondary: match &request.source {
                    SplitSpawnSource::Spawn { .. } => None,
                    SplitSpawnSource::MovePane { pane_id } => Some(*pane_id),
                },
            }),
            Self::KillPane(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::SetFocusedPane(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::MovePaneToNewTab(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::ActivatePaneDirection(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::EraseScrollbackRequest(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::AdjustPaneSize(request) => {
                RequestAuthority::PaneControl(PaneControlTargets::one(request.pane_id))
            }
            Self::ControlLeaseRequest(request) => RequestAuthority::ControlLease(request.pane_id),
            Self::ServiceDrainRequest(_) => RequestAuthority::HostSensitive,
            Self::SpawnV2(_)
            | Self::SetWindowWorkspace(_)
            | Self::TabTitleChanged(_)
            | Self::WindowTitleChanged(_)
            | Self::RenameWorkspace(_) => RequestAuthority::UntargetedMutation,
            Self::GetTlsCreds(_) | Self::GetClientList(_) | Self::SetPalette(_) => {
                RequestAuthority::HostSensitive
            }
            Self::Invalid { .. }
            | Self::ErrorResponse(_)
            | Self::Pong(_)
            | Self::ListPanesResponse(_)
            | Self::SpawnResponse(_)
            | Self::UnitResponse(_)
            | Self::SetClipboard(_)
            | Self::GetLinesResponse(_)
            | Self::GetPaneRenderChangesResponse(_)
            | Self::GetCodecVersionResponse(_)
            | Self::GetBuildIdentityResponse(_)
            | Self::GetTlsCredsResponse(_)
            | Self::LivenessResponse(_)
            | Self::SearchScrollbackResponse(_)
            | Self::PaneRemoved(_)
            | Self::NotifyAlert(_)
            | Self::WindowWorkspaceChanged(_)
            | Self::GetClientListResponse(_)
            | Self::GetImageCellResponse(_)
            | Self::MovePaneToNewTabResponse(_)
            | Self::GetPaneRenderableDimensionsResponse(_)
            | Self::PaneFocused(_)
            | Self::TabResized(_)
            | Self::TabAddedToWindow(_)
            | Self::GetPaneDirectionResponse(_)
            | Self::ControlLeaseResult(_)
            | Self::ControlSnapshot(_)
            | Self::ControlChanged(_)
            | Self::AttachRejected(_)
            | Self::ServiceDrainResult(_)
            | Self::SetClientIdResponse(_) => {
                return Err(InvalidPduDirection {
                    pdu: self.pdu_name(),
                })
            }
        };
        Ok(authority)
    }

    /// Returns true if this type of Pdu represents action taken
    /// directly by a user, rather than background traffic on
    /// a live connection
    pub fn is_user_input(&self) -> bool {
        matches!(
            self,
            Self::WriteToPane(_)
                | Self::SendKeyDown(_)
                | Self::SendMouseEvent(_)
                | Self::SendPaste(_)
                | Self::Resize(_)
                | Self::SetClipboard(_)
                | Self::SetPaneZoomed(_)
                | Self::SpawnV2(_)
        )
    }

    pub fn stream_decode(
        buffer: &mut Vec<u8>,
        context: DecodeContext,
        admission: &RuntimeAdmission,
    ) -> anyhow::Result<Option<AdmittedDecodedPdu>> {
        let mut cursor = Cursor::new(buffer.as_slice());
        match Self::decode(&mut cursor, context, admission) {
            Ok(decoded) => {
                let consumed = cursor.position() as usize;
                let remain = buffer.len() - consumed;
                // Remove `consumed` bytes from the start of the vec.
                // This is safe because the vec is just bytes and we are
                // constrained the offsets accordingly.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buffer.as_ptr().add(consumed),
                        buffer.as_mut_ptr(),
                        remain,
                    );
                }
                buffer.truncate(remain);
                Ok(Some(decoded))
            }
            Err(err) => {
                if let Some(ioerr) = err.root_cause().downcast_ref::<std::io::Error>() {
                    match ioerr.kind() {
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::WouldBlock => {
                            return Ok(None);
                        }
                        _ => {}
                    }
                } else {
                    log::error!("not an ioerror in stream_decode: {:?}", err);
                }
                Err(err)
            }
        }
    }

    pub fn try_read_and_decode<R: std::io::Read>(
        r: &mut R,
        buffer: &mut Vec<u8>,
        context: DecodeContext,
        admission: &RuntimeAdmission,
    ) -> anyhow::Result<Option<AdmittedDecodedPdu>> {
        loop {
            if let Some(decoded) = Self::stream_decode(buffer, context, admission)
                .context("stream_decode of buffer for PDU")?
            {
                return Ok(Some(decoded));
            }

            let mut buf = [0u8; 4096];
            let size = match r.read(&mut buf) {
                Ok(size) => size,
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(None);
                    }
                    return Err(err.into());
                }
            };
            if size == 0 {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "End Of File").into(),
                );
            }

            buffer.extend_from_slice(&buf[0..size]);
        }
    }

    pub fn pane_id(&self) -> Option<PaneId> {
        match self {
            Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse { pane_id, .. })
            | Pdu::SetPalette(SetPalette { pane_id, .. })
            | Pdu::NotifyAlert(NotifyAlert { pane_id, .. })
            | Pdu::SetClipboard(SetClipboard { pane_id, .. })
            | Pdu::PaneFocused(PaneFocused { pane_id })
            | Pdu::PaneRemoved(PaneRemoved { pane_id }) => Some(*pane_id),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct UnitResponse {}

/// A content-free server notification emitted when a newly accepted transport cannot be admitted.
///
/// It is sent with notification serial zero before the server closes the transport. Keeping this
/// payload empty prevents the pre-bootstrap rejection path from disclosing server state.
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct AttachRejected {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ErrorResponse {
    pub reason: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetCodecVersion {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetCodecVersionResponse {
    pub codec_vers: usize,
    pub version_string: String,
    pub executable_path: PathBuf,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, Debug)]
pub struct BuildIdentity {
    pub product: String,
    pub version: String,
    pub source_revision: Option<String>,
    pub source_dirty: Option<bool>,
    pub embedded_wezterm_revision: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetBuildIdentity {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetBuildIdentityResponse {
    pub identity: BuildIdentity,
}

/// Opaque identity issued by the server for one resumable attachment.
///
/// No client request accepts this value as authority. Clients may only compare identities
/// projected by control snapshots and changes.
#[derive(Clone, Copy, Deserialize, Serialize, Eq, Hash, Ord, PartialEq, PartialOrd, Debug)]
pub struct AttachmentIdentity(NonZeroU64);

impl AttachmentIdentity {
    pub fn from_server_sequence(sequence: NonZeroU64) -> Self {
        Self(sequence)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Secret capability used to resume one live attachment during its disconnect grace period.
///
/// It is intentionally absent from `Debug` output. The value is wire/runtime state only and must
/// never be persisted or displayed.
#[derive(Clone, Deserialize, Serialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AttachmentResumeToken([u8; 32]);

impl AttachmentResumeToken {
    pub fn from_random_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for AttachmentResumeToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AttachmentResumeToken([REDACTED])")
    }
}

impl Drop for AttachmentResumeToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub enum ControlLeaseAction {
    Acquire,
    Take,
    Release,
}

#[derive(Clone, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub struct ControlLeaseRequest {
    pub pane_id: PaneId,
    pub action: ControlLeaseAction,
}

#[derive(Clone, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub struct ActiveControlLease {
    pub pane_id: PaneId,
    pub controller: AttachmentIdentity,
}

#[derive(Clone, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub struct ControlLeaseState {
    pub sequence: u64,
    pub active: Vec<ActiveControlLease>,
}

#[derive(Clone, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub enum ControlLeaseResult {
    Acquired(ControlLeaseState),
    AlreadyController(ControlLeaseState),
    Observing(ControlLeaseState),
    Taken(ControlLeaseState),
    Released(ControlLeaseState),
    NotController(ControlLeaseState),
    Overloaded,
}

#[derive(Clone, Copy, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub enum ServiceDrainAction {
    Begin,
    Cancel,
}

#[derive(Clone, Copy, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub struct ServiceDrainRequest {
    pub action: ServiceDrainAction,
}

#[derive(Clone, Copy, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub struct ServiceDrainResult {
    pub draining: bool,
}

#[derive(Clone, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub struct ControlSnapshot {
    /// Identity of the attachment receiving this snapshot.
    ///
    /// This is comparison-only client state: no client request accepts a
    /// `AttachmentIdentity` as authority.
    pub attachment_identity: AttachmentIdentity,
    pub state: ControlLeaseState,
}

#[derive(Clone, Deserialize, Serialize, Eq, PartialEq, Debug)]
pub struct ControlChanged {
    pub state: ControlLeaseState,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Ping {}
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Pong {}

/// Requests a client certificate to authenticate against
/// the TLS based server
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetTlsCreds {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetTlsCredsResponse {
    /// The signing certificate
    pub ca_cert_pem: String,
    /// A client authentication certificate and private
    /// key, PEM encoded
    pub client_cert_pem: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ListPanes {}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ListPanesResponse {
    pub tabs: Vec<PaneNode>,
    pub tab_titles: Vec<String>,
    pub window_titles: HashMap<WindowId, String>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SpawnCommandError {
    #[error("spawn command argv must contain a program")]
    EmptyArgv,
    #[error("spawn command program must not be empty")]
    EmptyProgram,
    #[error("spawn command argument {index} is not valid UTF-8")]
    NonUtf8Argument { index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NonEmptyProgram(String);

impl NonEmptyProgram {
    pub fn new(program: String) -> Result<Self, SpawnCommandError> {
        if program.is_empty() {
            Err(SpawnCommandError::EmptyProgram)
        } else {
            Ok(Self(program))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for NonEmptyProgram {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let program = String::deserialize(deserializer)?;
        Self::new(program).map_err(serde::de::Error::custom)
    }
}

/// A command description that cannot carry process environment, umask, or tty policy.
#[derive(Clone, Deserialize, Serialize, PartialEq, Debug)]
pub enum EnvironmentFreeCommand {
    DefaultLoginShell,
    Program {
        program: NonEmptyProgram,
        args: Vec<String>,
    },
}

impl EnvironmentFreeCommand {
    pub fn try_from_argv<I, S>(argv: I) -> Result<Self, SpawnCommandError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut argv = argv.into_iter().enumerate();
        let (_, program) = argv.next().ok_or(SpawnCommandError::EmptyArgv)?;
        let program = program
            .as_ref()
            .to_str()
            .ok_or(SpawnCommandError::NonUtf8Argument { index: 0 })?;
        let program = NonEmptyProgram::new(program.to_string())?;
        let args = argv
            .map(|(index, arg)| {
                arg.as_ref()
                    .to_str()
                    .map(str::to_string)
                    .ok_or(SpawnCommandError::NonUtf8Argument { index })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::Program { program, args })
    }
}

/// A tab spawn cannot inherit an implicit current-pane domain.
#[derive(Clone, Deserialize, Serialize, PartialEq, Debug)]
pub enum TabSpawnDomain {
    DefaultDomain,
    DomainName(String),
    DomainId(usize),
}

/// Existing-window spawns carry no ignored size/workspace fields; new windows require both.
#[derive(Clone, Deserialize, Serialize, PartialEq, Debug)]
pub enum TabSpawnPlacement {
    ExistingWindow {
        window_id: WindowId,
    },
    NewWindow {
        size: TerminalSize,
        workspace: String,
    },
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Debug)]
pub enum SplitSpawnDomain {
    TargetPaneDomain,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Debug)]
pub enum SplitSpawnSource {
    Spawn {
        command: EnvironmentFreeCommand,
        command_dir: Option<String>,
    },
    MovePane {
        pane_id: PaneId,
    },
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SplitPane {
    pub target_pane_id: PaneId,
    pub split_request: SplitRequest,
    pub domain: SplitSpawnDomain,
    pub source: SplitSpawnSource,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MovePaneToNewTab {
    pub pane_id: PaneId,
    pub window_id: Option<WindowId>,
    pub workspace_for_new_window: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct MovePaneToNewTabResponse {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnV2 {
    pub domain: TabSpawnDomain,
    pub placement: TabSpawnPlacement,
    pub command: EnvironmentFreeCommand,
    pub command_dir: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct PaneRemoved {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct KillPane {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SpawnResponse {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub window_id: WindowId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WriteToPane {
    pub pane_id: PaneId,
    pub data: Vec<u8>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendPaste {
    pub pane_id: PaneId,
    pub data: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendKeyDown {
    pub pane_id: TabId,
    pub event: termwiz::input::KeyEvent,
    pub input_serial: InputSerial,
}

/// InputSerial is used to sequence input requests with output events.
/// It started life as a monotonic sequence number but evolved into
/// the number of milliseconds since the unix epoch.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, PartialOrd, Ord)]
pub struct InputSerial(u64);

impl InputSerial {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn now() -> Self {
        std::time::SystemTime::now().into()
    }

    pub fn elapsed_millis(&self) -> u64 {
        let now = InputSerial::now();
        now.0 - self.0
    }
}

impl From<std::time::SystemTime> for InputSerial {
    fn from(val: std::time::SystemTime) -> Self {
        let duration = val
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("SystemTime before unix epoch?");
        let millis: u64 = duration
            .as_millis()
            .try_into()
            .expect("millisecond count to fit in u64");
        InputSerial(millis)
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SendMouseEvent {
    pub pane_id: PaneId,
    pub event: wezterm_term::input::MouseEvent,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetClipboard {
    pub pane_id: PaneId,
    pub clipboard: Option<String>,
    pub selection: ClipboardSelection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetWindowWorkspace {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct RenameWorkspace {
    pub old_workspace: String,
    pub new_workspace: String,
}

/// This is used both as a notification from server->client
/// and as a configuration request from client->server when
/// the client's preferred configuration changes
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetPalette {
    pub pane_id: PaneId,
    pub palette: Box<ColorPalette>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct NotifyAlert {
    pub pane_id: PaneId,
    pub alert: Alert,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabAddedToWindow {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabResized {
    pub tab_id: TabId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct TabTitleChanged {
    pub tab_id: TabId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WindowTitleChanged {
    pub window_id: WindowId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct PaneFocused {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct WindowWorkspaceChanged {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetClientId {
    pub client_id: ClientId,
    pub is_proxy: bool,
    pub resume_token: Option<AttachmentResumeToken>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetClientIdResponse {
    pub resume_token: Option<AttachmentResumeToken>,
    pub control_snapshot: Option<ControlSnapshot>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetFocusedPane {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetClientList;

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetClientListResponse {
    pub clients: Vec<ClientInfo>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct Resize {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SetPaneZoomed {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub zoomed: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct AdjustPaneSize {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
    pub amount: usize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneDirectionResponse {
    pub pane_id: Option<PaneId>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct ActivatePaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderChanges {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderableDimensions {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderableDimensionsResponse {
    pub pane_id: PaneId,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct LivenessResponse {
    pub pane_id: PaneId,
    pub is_alive: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetPaneRenderChangesResponse {
    pub pane_id: PaneId,
    pub mouse_grabbed: bool,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
    pub dirty_lines: Vec<Range<StableRowIndex>>,
    pub title: String,
    pub working_dir: Option<SerdeUrl>,
    /// Lines that the server thought we'd almost certainly
    /// want to fetch as soon as we received this response
    pub bonus_lines: SerializedLines,

    pub input_serial: Option<InputSerial>,
    pub seqno: SequenceNo,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetLines {
    pub pane_id: PaneId,
    pub lines: Vec<Range<StableRowIndex>>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
struct CellCoordinates {
    line_idx: usize,
    cols: Range<usize>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
struct LineHyperlink {
    link: Hyperlink,
    coords: Vec<CellCoordinates>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SerializedImageCell {
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    // The following fields are taken from termwiz::image::ImageCell
    pub top_left: TextureCoordinate,
    pub bottom_right: TextureCoordinate,
    /// Image::data::hash() for the ImageCell::data field
    pub data_hash: [u8; 32],
    pub z_index: i32,
    pub padding_left: u16,
    pub padding_top: u16,
    pub padding_right: u16,
    pub padding_bottom: u16,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
}

/// What's all this?
/// Cells hold references to Arc<Hyperlink> and it is important to us to
/// maintain identity of the hyperlinks in the individual cells, while also
/// only sending a single copy of the associated URL.
/// This section of code extracts the hyperlinks from the cells and builds
/// up a mapping that can be used to restore the identity when the `lines()`
/// method is called.
#[derive(Deserialize, Serialize, PartialEq, Debug, Default)]
pub struct SerializedLines {
    lines: Vec<(StableRowIndex, Line)>,
    hyperlinks: Vec<LineHyperlink>,
    images: Vec<SerializedImageCell>,
}

impl SerializedLines {
    /// Reconsitute hyperlinks or other attributes that were decomposed for
    /// serialization, and return the line data.
    pub fn extract_data(self) -> (Vec<(StableRowIndex, Line)>, Vec<SerializedImageCell>) {
        let lines = if self.hyperlinks.is_empty() {
            self.lines
        } else {
            let mut lines = self.lines;

            for link in self.hyperlinks {
                let url = Arc::new(link.link);

                for coord in link.coords {
                    if let Some((_, line)) = lines.get_mut(coord.line_idx) {
                        if let Some(cells) =
                            line.cells_mut_for_attr_changes_only().get_mut(coord.cols)
                        {
                            for cell in cells {
                                cell.attrs_mut().set_hyperlink(Some(Arc::clone(&url)));
                            }
                        }
                    }
                }
            }

            lines
        };
        (lines, self.images)
    }
}

impl From<Vec<(StableRowIndex, Line)>> for SerializedLines {
    fn from(mut lines: Vec<(StableRowIndex, Line)>) -> Self {
        let mut hyperlinks = vec![];
        let mut images = vec![];

        for (line_idx, (stable_row_idx, line)) in lines.iter_mut().enumerate() {
            let mut current_link: Option<Arc<Hyperlink>> = None;
            let mut current_range = 0..0;

            for (x, cell) in line
                .cells_mut_for_attr_changes_only()
                .iter_mut()
                .enumerate()
            {
                // Unset the hyperlink on the cell, if any, and record that
                // in the hyperlinks data for later restoration.
                if let Some(link) = cell.attrs_mut().hyperlink().map(Arc::clone) {
                    cell.attrs_mut().set_hyperlink(None);
                    match current_link.as_ref() {
                        Some(current) if Arc::ptr_eq(current, &link) => {
                            // Continue the current streak
                            current_range = range_union(current_range, x..x + 1);
                        }
                        Some(prior) => {
                            // It's a different URL, push the current data and start a new one
                            hyperlinks.push(LineHyperlink {
                                link: (**prior).clone(),
                                coords: vec![CellCoordinates {
                                    line_idx,
                                    cols: current_range,
                                }],
                            });
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                        None => {
                            // Starting a new streak
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                    }
                } else if let Some(link) = current_link.take() {
                    // Wrap up a prior streak
                    hyperlinks.push(LineHyperlink {
                        link: (*link).clone(),
                        coords: vec![CellCoordinates {
                            line_idx,
                            cols: current_range,
                        }],
                    });
                    current_range = 0..0;
                }

                if let Some(cell_images) = cell.attrs().images() {
                    for imcell in cell_images {
                        let (padding_left, padding_top, padding_right, padding_bottom) =
                            imcell.padding();
                        images.push(SerializedImageCell {
                            line_idx: *stable_row_idx,
                            cell_idx: x,
                            top_left: imcell.top_left(),
                            bottom_right: imcell.bottom_right(),
                            z_index: imcell.z_index(),
                            padding_left,
                            padding_top,
                            padding_right,
                            padding_bottom,
                            image_id: imcell.image_id(),
                            placement_id: imcell.placement_id(),
                            data_hash: imcell.image_data().hash(),
                        });
                    }
                }
                cell.attrs_mut().clear_images();
            }
            if let Some(link) = current_link.take() {
                // Wrap up final streak
                hyperlinks.push(LineHyperlink {
                    link: (*link).clone(),
                    coords: vec![CellCoordinates {
                        line_idx,
                        cols: current_range,
                    }],
                });
            }
        }

        Self {
            lines,
            hyperlinks,
            images,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetLinesResponse {
    pub pane_id: PaneId,
    pub lines: SerializedLines,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct EraseScrollbackRequest {
    pub pane_id: PaneId,
    pub erase_mode: ScrollbackEraseMode,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SearchScrollbackRequest {
    pub pane_id: PaneId,
    pub pattern: mux::pane::Pattern,
    pub range: Range<StableRowIndex>,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct SearchScrollbackResponse {
    pub results: Vec<mux::pane::SearchResult>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCell {
    pub pane_id: PaneId,
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    pub data_hash: [u8; 32],
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCellResponse {
    pub pane_id: PaneId,
    pub data: Option<Arc<ImageData>>,
}

#[cfg(test)]
mod test {
    use super::*;

    struct HeaderThenPoison {
        header: Cursor<Vec<u8>>,
        body_reads: usize,
    }

    impl std::io::Read for HeaderThenPoison {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.header.position() < self.header.get_ref().len() as u64 {
                std::io::Read::read(&mut self.header, buf)
            } else {
                self.body_reads += 1;
                Err(std::io::Error::other("poison body was read"))
            }
        }
    }

    fn header_only(tag: PduTag, serial: u64, body_len: usize) -> Vec<u8> {
        let content_len = encoded_length(serial)
            .checked_add(encoded_length(tag.ident()))
            .and_then(|len| len.checked_add(body_len))
            .unwrap();
        let mut header = Vec::new();
        leb128::write::unsigned(&mut header, content_len as u64).unwrap();
        leb128::write::unsigned(&mut header, serial).unwrap();
        leb128::write::unsigned(&mut header, tag.ident()).unwrap();
        header
    }

    fn content_len_for_complete_frame(frame_len: usize) -> usize {
        (1..=10)
            .find_map(|prefix_len| {
                let content_len = frame_len.checked_sub(prefix_len)?;
                (encoded_length(content_len as u64) == prefix_len).then_some(content_len)
            })
            .expect("frame length must have a matching leb128 prefix")
    }

    fn admission() -> Arc<RuntimeAdmission> {
        RuntimeAdmission::new(wezterm_runtime_admission::RuntimeRole::Client).unwrap()
    }

    #[test]
    fn environment_free_command_requires_an_explicit_nonempty_program() {
        assert_eq!(
            EnvironmentFreeCommand::try_from_argv(Vec::<String>::new()).unwrap_err(),
            SpawnCommandError::EmptyArgv
        );
        assert_eq!(
            EnvironmentFreeCommand::try_from_argv([""]).unwrap_err(),
            SpawnCommandError::EmptyProgram
        );
        assert_eq!(
            EnvironmentFreeCommand::try_from_argv(["bash", "-lc", "echo ready"]).unwrap(),
            EnvironmentFreeCommand::Program {
                program: NonEmptyProgram::new("bash".to_string()).unwrap(),
                args: vec!["-lc".to_string(), "echo ready".to_string()],
            }
        );
    }

    #[test]
    fn hostile_empty_program_fails_during_deserialization() {
        let admission = admission();
        let (encoded, compressed, _permit) = serialize(&String::new(), &admission).unwrap();
        let error = deserialize::<NonEmptyProgram, _>(encoded.as_slice(), compressed).unwrap_err();
        assert!(error.to_string().contains("program must not be empty"));
    }

    fn assert_decoded(decoded: AdmittedDecodedPdu, serial: u64, pdu: Pdu) {
        assert_eq!(decoded.serial(), serial);
        assert_eq!(decoded.pdu(), &pdu);
    }

    #[test]
    fn test_frame() {
        let mut encoded = Vec::new();
        encode_raw(1, 0x42, b"hello", false, &mut encoded).unwrap();
        assert_eq!(&encoded, b"\x07\x42\x01hello");
        let admission = admission();
        let mut encoded = encoded.as_slice();
        let header = Pdu::read_header(&mut encoded).unwrap();
        assert_eq!(header.tag(), PduTag::Ping);
        assert_eq!(header.serial(), 0x42);
        let body = header
            .validate(
                DecodeContext::client_to_server_request(ClientRequestPhase::Established),
                &admission,
            )
            .unwrap();
        assert_eq!(read_body(&mut encoded, &body).unwrap(), b"hello");
    }

    #[test]
    fn test_frame_lengths() {
        for (serial, target_len) in (1..).zip([128, 247, 256, 65536, 16777216].iter()) {
            let mut payload = Vec::with_capacity(*target_len);
            payload.resize(*target_len, b'a');
            let mut encoded = Vec::new();
            encode_raw(1, serial, payload.as_slice(), false, &mut encoded).unwrap();
            let admission = admission();
            let mut encoded = encoded.as_slice();
            let header = Pdu::read_header(&mut encoded).unwrap();
            assert_eq!(header.tag(), PduTag::Ping);
            assert_eq!(header.serial(), serial);
            let body = header
                .validate(
                    DecodeContext::client_to_server_request(ClientRequestPhase::Established),
                    &admission,
                )
                .unwrap();
            assert_eq!(read_body(&mut encoded, &body).unwrap(), payload);
        }
    }

    #[test]
    fn test_pdu_ping() {
        let mut encoded = Vec::new();
        let admission = admission();
        Pdu::Ping(Ping {})
            .encode(&mut encoded, 0x40, &admission)
            .unwrap();
        assert_eq!(&encoded, &[2, 0x40, 1]);
        assert_decoded(
            Pdu::decode(
                encoded.as_slice(),
                DecodeContext::client_to_server_request(ClientRequestPhase::Established),
                &admission,
            )
            .unwrap(),
            0x40,
            Pdu::Ping(Ping {}),
        );
    }

    #[test]
    fn stream_decode() {
        let mut encoded = Vec::new();
        let admission = admission();
        Pdu::Ping(Ping {})
            .encode(&mut encoded, 0x1, &admission)
            .unwrap();
        Pdu::Pong(Pong {})
            .encode(&mut encoded, 0x2, &admission)
            .unwrap();
        assert_eq!(encoded.len(), 6);

        let mut cursor = Cursor::new(encoded.as_slice());
        let mut read_buffer = Vec::new();

        assert_decoded(
            Pdu::try_read_and_decode(
                &mut cursor,
                &mut read_buffer,
                DecodeContext::client_to_server_request(ClientRequestPhase::Established),
                &admission,
            )
            .unwrap()
            .unwrap(),
            1,
            Pdu::Ping(Ping {}),
        );
        assert_decoded(
            Pdu::try_read_and_decode(
                &mut cursor,
                &mut read_buffer,
                DecodeContext::server_to_client_response(Some(PduTag::Pong)),
                &admission,
            )
            .unwrap()
            .unwrap(),
            2,
            Pdu::Pong(Pong {}),
        );
        let err = Pdu::try_read_and_decode(
            &mut cursor,
            &mut read_buffer,
            DecodeContext::server_to_client_response(Some(PduTag::Pong)),
            &admission,
        )
        .unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_pdu_ping_base91() {
        let mut encoded = Vec::new();
        let admission = admission();
        {
            let mut encoder = base91::Base91Encoder::new(&mut encoded);
            Pdu::Ping(Ping {})
                .encode(&mut encoder, 0x41, &admission)
                .unwrap();
        }
        assert_eq!(&encoded, &[60, 67, 75, 65]);
        let decoded = base91::decode(&encoded);
        assert_decoded(
            Pdu::decode(
                decoded.as_slice(),
                DecodeContext::client_to_server_request(ClientRequestPhase::Established),
                &admission,
            )
            .unwrap(),
            0x41,
            Pdu::Ping(Ping {}),
        );
    }

    #[test]
    fn test_pdu_pong() {
        let mut encoded = Vec::new();
        let admission = admission();
        Pdu::Pong(Pong {})
            .encode(&mut encoded, 0x42, &admission)
            .unwrap();
        assert_eq!(&encoded, &[2, 0x42, 2]);
        assert_decoded(
            Pdu::decode(
                encoded.as_slice(),
                DecodeContext::server_to_client_response(Some(PduTag::Pong)),
                &admission,
            )
            .unwrap(),
            0x42,
            Pdu::Pong(Pong {}),
        );
    }

    #[test]
    fn test_bogus_pdu() {
        let mut encoded = Vec::new();
        encode_raw(0xdeadbeef, 0x42, b"hello", false, &mut encoded).unwrap();
        let admission = admission();
        assert!(Pdu::decode(
            encoded.as_slice(),
            DecodeContext::client_to_server_request(ClientRequestPhase::Established),
            &admission,
        )
        .is_err());
    }

    #[test]
    fn complete_wire_frame_boundary_is_exact() {
        let content_len = content_len_for_complete_frame(MAX_WIRE_FRAME_BYTES);
        let mut boundary = Vec::new();
        leb128::write::unsigned(&mut boundary, content_len as u64).unwrap();
        leb128::write::unsigned(&mut boundary, 1).unwrap();
        leb128::write::unsigned(&mut boundary, PduTag::Ping.ident()).unwrap();
        let header = Pdu::read_header(&mut boundary.as_slice()).unwrap();
        assert_eq!(header.tag(), PduTag::Ping);

        let content_len = content_len_for_complete_frame(MAX_WIRE_FRAME_BYTES + 1);
        let mut oversized = Vec::new();
        leb128::write::unsigned(&mut oversized, content_len as u64).unwrap();
        leb128::write::unsigned(&mut oversized, 1).unwrap();
        leb128::write::unsigned(&mut oversized, PduTag::Ping.ident()).unwrap();
        let error = Pdu::read_header(&mut oversized.as_slice()).unwrap_err();
        assert!(error.to_string().contains("wire frame exceeds"));
    }

    #[test]
    fn unknown_identifier_is_rejected_before_body_read() {
        let mut encoded = Vec::new();
        let unknown_ident = 0xdead_beef;
        let content_len = encoded_length(1) + encoded_length(unknown_ident);
        leb128::write::unsigned(&mut encoded, content_len as u64).unwrap();
        leb128::write::unsigned(&mut encoded, 1).unwrap();
        leb128::write::unsigned(&mut encoded, unknown_ident).unwrap();
        let admission = admission();
        let error = Pdu::read_header(&mut encoded.as_slice()).unwrap_err();
        assert!(error.to_string().contains("unknown PDU identifier"));
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn wrong_direction_does_not_read_the_body() {
        let mut reader = HeaderThenPoison {
            header: Cursor::new(header_only(PduTag::Pong, 1, 1)),
            body_reads: 0,
        };
        let admission = admission();
        let header = Pdu::read_header(&mut reader).unwrap();
        assert!(header
            .validate(
                DecodeContext::client_to_server_request(ClientRequestPhase::Established),
                &admission,
            )
            .is_err());
        assert_eq!(reader.body_reads, 0);
    }

    #[test]
    fn phase_invalid_client_requests_fail_before_body_admission() {
        for (tag, phase) in [
            (PduTag::WriteToPane, ClientRequestPhase::Bootstrap),
            (PduTag::SetClientId, ClientRequestPhase::Established),
        ] {
            let mut reader = HeaderThenPoison {
                header: Cursor::new(header_only(tag, 1, 1)),
                body_reads: 0,
            };
            let admission = admission();
            let header = Pdu::read_header(&mut reader).unwrap();
            let error = header
                .validate(DecodeContext::client_to_server_request(phase), &admission)
                .unwrap_err();

            assert!(error
                .to_string()
                .contains("invalid for client request phase"));
            assert_eq!(reader.body_reads, 0);
            assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
        }
    }

    #[test]
    fn correlated_response_rejects_zero_missing_and_wrong_tag_before_body() {
        let admission = admission();
        for (serial, expected, message) in [
            (0, Some(PduTag::Pong), "serial zero"),
            (1, None, "no corresponding promise"),
            (1, Some(PduTag::UnitResponse), "expected UnitResponse"),
        ] {
            let mut reader = HeaderThenPoison {
                header: Cursor::new(header_only(PduTag::Pong, serial, 1)),
                body_reads: 0,
            };
            let header = Pdu::read_header(&mut reader).unwrap();
            let error = header
                .validate(
                    DecodeContext::server_to_client_response(expected),
                    &admission,
                )
                .unwrap_err();
            let rendered = format!("{error:#}");
            assert!(rendered.contains(message), "{}", rendered);
            assert_eq!(reader.body_reads, 0);
        }

        let mut error_reader = HeaderThenPoison {
            header: Cursor::new(header_only(PduTag::ErrorResponse, 1, 1)),
            body_reads: 0,
        };
        let error_header = Pdu::read_header(&mut error_reader).unwrap();
        error_header
            .validate(
                DecodeContext::server_to_client_response(Some(PduTag::Pong)),
                &admission,
            )
            .unwrap();
        assert_eq!(error_reader.body_reads, 0);
    }

    #[test]
    fn render_poll_manifest_distinguishes_correlated_liveness_from_pushed_changes() {
        assert_eq!(
            PduTag::GetPaneRenderChanges.expected_response(),
            Some(PduTag::LivenessResponse)
        );

        let admission = admission();
        let pushed_changes_frame = header_only(PduTag::GetPaneRenderChangesResponse, 0, 1);
        let mut pushed_changes = pushed_changes_frame.as_slice();
        Pdu::read_header(&mut pushed_changes)
            .unwrap()
            .validate(DecodeContext::server_to_client_notification(), &admission)
            .unwrap();

        let liveness_frame = header_only(PduTag::LivenessResponse, 1, 1);
        let mut liveness = liveness_frame.as_slice();
        Pdu::read_header(&mut liveness)
            .unwrap()
            .validate(
                DecodeContext::server_to_client_response(Some(PduTag::LivenessResponse)),
                &admission,
            )
            .unwrap();
    }

    #[test]
    fn rpc_response_releases_wire_admission_but_keeps_heap_until_explicit_acceptance() {
        let admission = admission();
        let mut encoded = Vec::new();
        Pdu::ServiceDrainResult(ServiceDrainResult { draining: true })
            .encode(&mut encoded, 1, &admission)
            .unwrap();

        let decoded = Pdu::decode(
            encoded.as_slice(),
            DecodeContext::server_to_client_response(Some(PduTag::ServiceDrainResult)),
            &admission,
        )
        .unwrap();
        assert_eq!(
            admission.byte_usage(ByteClass::DecodeWorking),
            MAX_DECODE_METADATA_HEAP_ENVELOPE_BYTES_PER_PDU
        );

        let response = decoded
            .into_rpc_response()
            .unwrap()
            .try_map(|pdu| match pdu {
                Pdu::ServiceDrainResult(result) => Ok(result),
                unexpected => bail!("unexpected response {unexpected:?}"),
            })
            .unwrap();
        assert_eq!(response.serial(), 1);
        assert_eq!(
            admission.byte_usage(ByteClass::DecodeWorking),
            MAX_DECODE_METADATA_HEAP_ENVELOPE_BYTES_PER_PDU
        );

        let _result = response.into_inner();
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn control_response_remains_admissible_behind_four_large_decoded_values() {
        let admission = admission();
        let large = (0..4)
            .map(|_| {
                admission
                    .try_bytes(
                        ByteClass::DecodeWorking,
                        MAX_DECODE_HEAP_ENVELOPE_BYTES_PER_PDU,
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut encoded = Vec::new();
        Pdu::ControlLeaseResult(ControlLeaseResult::Overloaded)
            .encode(&mut encoded, 1, &admission)
            .unwrap();

        let response = Pdu::decode(
            encoded.as_slice(),
            DecodeContext::server_to_client_response(Some(PduTag::ControlLeaseResult)),
            &admission,
        )
        .unwrap()
        .into_rpc_response()
        .unwrap();

        assert!(matches!(
            response.value(),
            Pdu::ControlLeaseResult(ControlLeaseResult::Overloaded)
        ));
        assert!(
            admission.byte_usage(ByteClass::DecodeWorking)
                >= large.len() * MAX_DECODE_HEAP_ENVELOPE_BYTES_PER_PDU
                    + MAX_DECODE_METADATA_HEAP_ENVELOPE_BYTES_PER_PDU
        );
        drop(response);
        drop(large);
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn alert_notification_remains_admissible_behind_four_large_decoded_values() {
        let admission = admission();
        let large = (0..4)
            .map(|_| {
                admission
                    .try_bytes(
                        ByteClass::DecodeWorking,
                        MAX_DECODE_HEAP_ENVELOPE_BYTES_PER_PDU,
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut encoded = Vec::new();
        Pdu::NotifyAlert(NotifyAlert {
            pane_id: 7,
            alert: Alert::Bell,
        })
        .encode(&mut encoded, 0, &admission)
        .unwrap();

        let notification = Pdu::decode(
            encoded.as_slice(),
            DecodeContext::server_to_client_notification(),
            &admission,
        )
        .unwrap()
        .into_notification()
        .unwrap();

        assert!(matches!(
            notification.pdu(),
            Pdu::NotifyAlert(NotifyAlert {
                pane_id: 7,
                alert: Alert::Bell
            })
        ));
        drop(notification);
        drop(large);
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn rejected_rpc_mapping_releases_decode_admission() {
        let admission = admission();
        let mut encoded = Vec::new();
        Pdu::Pong(Pong {})
            .encode(&mut encoded, 1, &admission)
            .unwrap();
        let response = Pdu::decode(
            encoded.as_slice(),
            DecodeContext::server_to_client_response(Some(PduTag::Pong)),
            &admission,
        )
        .unwrap()
        .into_rpc_response()
        .unwrap();

        let result: anyhow::Result<AdmittedRpcResponse<()>> =
            response.try_map(|_| bail!("reject typed response"));
        assert!(result.is_err());
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn notification_conversion_proves_zero_serial_and_retains_admission() {
        let admission = admission();
        let mut encoded = Vec::new();
        Pdu::PaneRemoved(PaneRemoved { pane_id: 7 })
            .encode(&mut encoded, 0, &admission)
            .unwrap();
        let notification = Pdu::decode(
            encoded.as_slice(),
            DecodeContext::server_to_client_notification(),
            &admission,
        )
        .unwrap()
        .into_notification()
        .unwrap();

        assert_eq!(notification.serial().get(), 0);
        assert!(admission.byte_usage(ByteClass::DecodeWorking) > 0);
        drop(notification);
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn decoded_correlation_cannot_cross_notification_and_response_boundaries() {
        let admission = admission();
        let mut response = Vec::new();
        Pdu::Pong(Pong {})
            .encode(&mut response, 1, &admission)
            .unwrap();
        assert!(Pdu::decode(
            response.as_slice(),
            DecodeContext::server_to_client_response(Some(PduTag::Pong)),
            &admission,
        )
        .unwrap()
        .into_notification()
        .is_err());
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);

        let mut notification = Vec::new();
        Pdu::PaneRemoved(PaneRemoved { pane_id: 7 })
            .encode(&mut notification, 0, &admission)
            .unwrap();
        assert!(Pdu::decode(
            notification.as_slice(),
            DecodeContext::server_to_client_notification(),
            &admission,
        )
        .unwrap()
        .into_rpc_response()
        .is_err());
        assert_eq!(admission.byte_usage(ByteClass::DecodeWorking), 0);
    }

    #[test]
    fn attach_rejected_is_a_content_free_server_notification() {
        let admission = admission();
        let mut encoded = Vec::new();
        Pdu::AttachRejected(AttachRejected {})
            .encode(&mut encoded, 0, &admission)
            .unwrap();

        let decoded = Pdu::decode(
            encoded.as_slice(),
            DecodeContext::server_to_client_notification(),
            &admission,
        )
        .unwrap()
        .into_notification()
        .unwrap();
        assert!(matches!(
            decoded.pdu(),
            Pdu::AttachRejected(AttachRejected {})
        ));
        assert_eq!(decoded.serial().get(), 0);

        let mut header = encoded.as_slice();
        let header = Pdu::read_header(&mut header).unwrap();
        assert!(header
            .validate(
                DecodeContext::client_to_server_request(ClientRequestPhase::Bootstrap),
                &admission,
            )
            .is_err());
    }

    #[test]
    fn every_manifest_request_targets_a_response_tag() {
        for tag in PduTag::ALL {
            let Some(response) = tag.expected_response() else {
                continue;
            };
            assert_eq!(
                response.header_policy(),
                PduHeaderPolicy::Response,
                "{} expects non-response tag {}",
                tag.name(),
                response.name()
            );
        }
    }

    #[test]
    fn every_client_request_tag_has_phase_classification() {
        for &tag in PduTag::ALL {
            let is_request = matches!(
                tag.header_policy(),
                PduHeaderPolicy::Request { .. } | PduHeaderPolicy::RequestOrNotification { .. }
            );
            let expected_bootstrap = matches!(
                tag,
                PduTag::Ping
                    | PduTag::GetCodecVersion
                    | PduTag::GetBuildIdentity
                    | PduTag::GetTlsCreds
                    | PduTag::SetClientId
            );
            let expected_established = is_request && tag != PduTag::SetClientId;

            assert_eq!(
                tag.allows_client_request_phase(ClientRequestPhase::Bootstrap),
                expected_bootstrap,
                "{} has the wrong bootstrap classification",
                tag.name()
            );
            assert_eq!(
                tag.allows_client_request_phase(ClientRequestPhase::Established),
                expected_established,
                "{} has the wrong established classification",
                tag.name()
            );
            assert_eq!(
                expected_bootstrap || expected_established,
                is_request,
                "{} is missing a client request phase classification",
                tag.name()
            );
        }
    }

    #[test]
    fn projected_attachment_identity_is_never_client_request_authority() {
        let identity = AttachmentIdentity::from_server_sequence(NonZeroU64::new(7).unwrap());
        let snapshot = Pdu::ControlSnapshot(ControlSnapshot {
            attachment_identity: identity,
            state: ControlLeaseState {
                sequence: 0,
                active: vec![],
            },
        });

        assert!(snapshot.request_operation().is_err());
        assert!(snapshot.request_authority().is_err());
        assert!(!PduTag::ControlSnapshot.allows_client_request_phase(ClientRequestPhase::Bootstrap));
        assert!(
            !PduTag::ControlSnapshot.allows_client_request_phase(ClientRequestPhase::Established)
        );
    }

    #[test]
    fn valid_header_reaches_the_body_reader() {
        let mut reader = HeaderThenPoison {
            header: Cursor::new(header_only(PduTag::Ping, 1, 1)),
            body_reads: 0,
        };
        let admission = admission();
        let header = Pdu::read_header(&mut reader).unwrap();
        let body = header
            .validate(
                DecodeContext::client_to_server_request(ClientRequestPhase::Established),
                &admission,
            )
            .unwrap();
        let error = Pdu::decode_body(&mut reader, body, &admission).unwrap_err();
        assert!(format!("{error:#}").contains("poison body was read"));
        assert_eq!(reader.body_reads, 1);
    }

    #[test]
    fn trailing_deserialized_body_bytes_are_rejected() {
        let mut encoded = Vec::new();
        encode_raw(PduTag::Ping.ident(), 1, &[0xaa], false, &mut encoded).unwrap();
        let admission = admission();
        let error = Pdu::decode(
            encoded.as_slice(),
            DecodeContext::client_to_server_request(ClientRequestPhase::Established),
            &admission,
        )
        .unwrap_err();
        assert!(error.to_string().contains("trailing bytes"));
    }
}
