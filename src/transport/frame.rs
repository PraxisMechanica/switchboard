//! Length-prefixed JSON wire frame types and synchronous/asynchronous codecs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{TransportError, MAX_FRAME_BYTES, PROTOCOL_VERSION};

/// High-level protocol message discriminator carried in a [`WireFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameKind {
    Knock,
    KnockDecision,
    CallRequest,
    CallResponse,
    Error,
}

/// Versioned protocol envelope transmitted over a single bidirectional stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireFrame {
    pub version: u16,
    pub kind: FrameKind,
    pub payload: Value,
}

impl WireFrame {
    /// Creates a frame stamped with [`PROTOCOL_VERSION`].
    pub fn new(kind: FrameKind, payload: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            kind,
            payload,
        }
    }
}

/// Encodes a [`WireFrame`] into a 4-byte big-endian length prefix followed by UTF-8 JSON bytes.
pub fn encode_frame(frame: &WireFrame) -> Result<Vec<u8>, TransportError> {
    if frame.version != PROTOCOL_VERSION {
        return Err(TransportError::UnsupportedVersion(frame.version));
    }

    let body =
        serde_json::to_vec(frame).map_err(|err| TransportError::InvalidFrame(err.to_string()))?;

    if body.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(body.len()));
    }

    let body_len =
        u32::try_from(body.len()).map_err(|_| TransportError::FrameTooLarge(body.len()))?;

    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&body_len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decodes a single length-prefixed [`WireFrame`] from a complete byte slice.
pub fn decode_frame(bytes: &[u8]) -> Result<WireFrame, TransportError> {
    if bytes.len() < 4 {
        return Err(TransportError::InvalidFrame(
            "frame shorter than 4-byte length prefix".to_string(),
        ));
    }

    let len_bytes: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
    let declared_len = u32::from_be_bytes(len_bytes) as usize;

    if declared_len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(declared_len));
    }

    let body = &bytes[4..];
    if body.len() != declared_len {
        return Err(TransportError::InvalidFrame(format!(
            "frame length mismatch: header declares {declared_len} bytes, got {}",
            body.len()
        )));
    }

    decode_body(body)
}

/// Writes a length-prefixed [`WireFrame`] onto an async stream and flushes it.
pub async fn write_frame<W>(writer: &mut W, frame: &WireFrame) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let encoded = encode_frame(frame)?;
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads a single length-prefixed [`WireFrame`] from an async stream, enforcing [`MAX_FRAME_BYTES`].
pub async fn read_frame<R>(reader: &mut R) -> Result<WireFrame, TransportError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;

    let declared_len = u32::from_be_bytes(len_buf) as usize;
    if declared_len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(declared_len));
    }

    let mut body = vec![0u8; declared_len];
    reader.read_exact(&mut body).await?;

    decode_body(&body)
}

fn decode_body(body: &[u8]) -> Result<WireFrame, TransportError> {
    let frame: WireFrame = serde_json::from_slice(body)
        .map_err(|err| TransportError::InvalidFrame(err.to_string()))?;

    if frame.version != PROTOCOL_VERSION {
        return Err(TransportError::UnsupportedVersion(frame.version));
    }

    Ok(frame)
}
