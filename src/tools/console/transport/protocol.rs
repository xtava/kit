use std::io;

use serde_json::Error as JsonError;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use wezterm_codec::BuildIdentity;

const CONNECT_LINE: &[u8] = b"KIT-CONSOLE/1 CONNECT\n";
const READY_PREFIX: &[u8] = b"KIT-CONSOLE/1 READY ";
const ERROR_PREFIX: &[u8] = b"KIT-CONSOLE/1 ERROR ";

/// Maximum request line size, including its trailing newline.
pub(crate) const MAX_REQUEST_LINE_BYTES: usize = 4 * 1024;

/// Maximum response line size, including its trailing newline.
pub(crate) const MAX_RESPONSE_LINE_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientRequest {
    Connect,
}

impl ClientRequest {
    pub(crate) fn encode(self) -> Vec<u8> {
        match self {
            Self::Connect => CONNECT_LINE.to_vec(),
        }
    }

    pub(crate) fn parse_line(line: &[u8]) -> Result<Self, ProtocolError> {
        validate_line(line, MAX_REQUEST_LINE_BYTES)?;
        if line == CONNECT_LINE {
            Ok(Self::Connect)
        } else {
            Err(ProtocolError::InvalidRequest)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GatewayErrorCode {
    Protocol,
    Auth,
    Unavailable,
}

impl GatewayErrorCode {
    fn label(self) -> &'static [u8] {
        match self {
            Self::Protocol => b"PROTOCOL",
            Self::Auth => b"AUTH",
            Self::Unavailable => b"UNAVAILABLE",
        }
    }

    fn parse(label: &[u8]) -> Result<Self, ProtocolError> {
        match label {
            b"PROTOCOL" => Ok(Self::Protocol),
            b"AUTH" => Ok(Self::Auth),
            b"UNAVAILABLE" => Ok(Self::Unavailable),
            _ => Err(ProtocolError::InvalidResponse),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ServerResponse {
    Ready { build: BuildIdentity },
    Error(GatewayErrorCode),
}

impl ServerResponse {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut line = match self {
            Self::Ready { build } => {
                let build = serde_json::to_vec(build).map_err(ProtocolError::EncodeBuild)?;
                let mut line = Vec::with_capacity(READY_PREFIX.len() + build.len() + 1);
                line.extend_from_slice(READY_PREFIX);
                line.extend_from_slice(&build);
                line
            }
            Self::Error(code) => {
                let mut line = Vec::with_capacity(ERROR_PREFIX.len() + code.label().len() + 1);
                line.extend_from_slice(ERROR_PREFIX);
                line.extend_from_slice(code.label());
                line
            }
        };
        line.push(b'\n');
        if line.len() > MAX_RESPONSE_LINE_BYTES {
            return Err(ProtocolError::LineTooLong { max_bytes: MAX_RESPONSE_LINE_BYTES });
        }
        Ok(line)
    }

    pub(crate) fn parse_line(line: &[u8]) -> Result<Self, ProtocolError> {
        validate_line(line, MAX_RESPONSE_LINE_BYTES)?;
        let payload = &line[..line.len() - 1];
        if let Some(build) = payload.strip_prefix(READY_PREFIX) {
            if build.is_empty() {
                return Err(ProtocolError::InvalidResponse);
            }
            let response = Self::Ready {
                build: serde_json::from_slice(build).map_err(ProtocolError::DecodeBuild)?,
            };
            if response.encode()?.as_slice() != line {
                return Err(ProtocolError::InvalidResponse);
            }
            return Ok(response);
        }
        if let Some(code) = payload.strip_prefix(ERROR_PREFIX) {
            return Ok(Self::Error(GatewayErrorCode::parse(code)?));
        }
        Err(ProtocolError::InvalidResponse)
    }
}

#[derive(Debug, Error)]
pub(crate) enum ProtocolError {
    #[error("Console gateway line exceeded the {max_bytes}-byte limit")]
    LineTooLong { max_bytes: usize },
    #[error("Console gateway line was not newline terminated")]
    MissingTerminator,
    #[error("Console gateway line used invalid framing")]
    InvalidFraming,
    #[error("invalid Console gateway request")]
    InvalidRequest,
    #[error("invalid Console gateway response")]
    InvalidResponse,
    #[error("encode Console gateway build identity: {0}")]
    EncodeBuild(JsonError),
    #[error("decode Console gateway build identity: {0}")]
    DecodeBuild(JsonError),
}

#[derive(Debug, Error)]
pub(crate) enum ReadLineError {
    #[error("read Console gateway protocol line: {0}")]
    Io(#[from] io::Error),
    #[error("Console gateway closed before sending a complete protocol line")]
    UnexpectedEof,
    #[error("Console gateway line exceeded the {max_bytes}-byte limit")]
    LineTooLong { max_bytes: usize },
}

/// Read exactly one bounded line without buffering bytes past its newline.
///
/// The byte-at-a-time boundary is intentional: the bytes immediately after a successful handshake
/// belong to the opaque mux stream and must remain unread for the relay.
pub(crate) async fn read_bounded_line<R>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>, ReadLineError>
where
    R: AsyncRead + Unpin,
{
    let mut line = Vec::with_capacity(CONNECT_LINE.len());
    let mut byte = [0u8; 1];
    loop {
        let count = reader.read(&mut byte).await?;
        if count == 0 {
            return Err(ReadLineError::UnexpectedEof);
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(line);
        }
        if line.len() >= max_bytes {
            return Err(ReadLineError::LineTooLong { max_bytes });
        }
    }
}

fn validate_line(line: &[u8], max_bytes: usize) -> Result<(), ProtocolError> {
    if line.len() > max_bytes {
        return Err(ProtocolError::LineTooLong { max_bytes });
    }
    if !line.ends_with(b"\n") {
        return Err(ProtocolError::MissingTerminator);
    }
    if line[..line.len() - 1].iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(ProtocolError::InvalidFraming);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn build() -> BuildIdentity {
        BuildIdentity {
            product: "kit-console".to_owned(),
            version: "1.2.3".to_owned(),
            source_revision: Some("a".repeat(40)),
            source_dirty: Some(false),
            embedded_wezterm_revision: Some("b".repeat(40)),
        }
    }

    #[test]
    fn connect_request_is_one_exact_versioned_line() {
        let encoded = ClientRequest::Connect.encode();
        assert_eq!(encoded, b"KIT-CONSOLE/1 CONNECT\n");
        assert_eq!(ClientRequest::parse_line(&encoded).unwrap(), ClientRequest::Connect);

        for invalid in [
            b"KIT-CONSOLE/1 CONNECT".as_slice(),
            b"KIT-CONSOLE/1 CONNECT\r\n".as_slice(),
            b"KIT-CONSOLE/2 CONNECT\n".as_slice(),
            b"KIT-CONSOLE/1 PROBE\n".as_slice(),
        ] {
            assert!(ClientRequest::parse_line(invalid).is_err());
        }
    }

    #[test]
    fn ready_response_round_trips_build_identity_on_one_bounded_line() {
        let response = ServerResponse::Ready { build: build() };
        let encoded = response.encode().unwrap();
        assert!(encoded.starts_with(b"KIT-CONSOLE/1 READY {"));
        assert!(encoded.ends_with(b"}\n"));
        assert!(encoded.len() <= MAX_RESPONSE_LINE_BYTES);
        assert_eq!(ServerResponse::parse_line(&encoded).unwrap(), response);
    }

    #[test]
    fn error_responses_have_only_the_three_machine_codes() {
        for code in
            [GatewayErrorCode::Protocol, GatewayErrorCode::Auth, GatewayErrorCode::Unavailable]
        {
            let response = ServerResponse::Error(code);
            let encoded = response.encode().unwrap();
            assert_eq!(ServerResponse::parse_line(&encoded).unwrap(), response);
        }
        assert!(ServerResponse::parse_line(b"KIT-CONSOLE/1 ERROR OTHER\n").is_err());
        assert!(ServerResponse::parse_line(b"KIT-CONSOLE/1 ERROR AUTH\r\n").is_err());
    }

    #[tokio::test]
    async fn bounded_reader_leaves_the_first_mux_bytes_unconsumed() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        writer.write_all(b"KIT-CONSOLE/1 CONNECT\nmux").await.unwrap();

        let line = read_bounded_line(&mut reader, MAX_REQUEST_LINE_BYTES).await.unwrap();
        let mut mux = [0u8; 3];
        reader.read_exact(&mut mux).await.unwrap();

        assert_eq!(line, ClientRequest::Connect.encode());
        assert_eq!(&mux, b"mux");
    }

    #[tokio::test]
    async fn bounded_reader_rejects_a_line_before_it_can_grow_past_the_limit() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_all(b"12345").await.unwrap();
        assert!(matches!(
            read_bounded_line(&mut reader, 4).await,
            Err(ReadLineError::LineTooLong { max_bytes: 4 })
        ));
    }
}
