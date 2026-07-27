// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use bytes::{BufMut, Bytes};
use iroh::endpoint::{AckFrequencyConfig, QuicTransportConfig, RecvStream, SendStream};
use prost::Message;
use tokio::io::AsyncReadExt;
use wire::PackChunkSpool;

use crate::{Result, TransportError};

pub const ALPN: &[u8] = b"heddle-sync/3";
pub const METHOD_LIST_REFS: &str = "/heddle.api.v1alpha1.RepoSyncService/ListRefs";
pub const METHOD_PULL: &str = "/heddle.api.v1alpha1.RepoSyncService/Pull";
pub const METHOD_WIRE_BENCHMARK: &str = "/heddle.experiment.Transport/BenchmarkWire";

const MAX_CONTROL_FRAME: usize = 8 * 1024 * 1024;
const MAX_METHOD_PATH: usize = 1024;
const DATA_CHUNK_SIZE: usize = 1024 * 1024;
const FILE_SEND_CHUNK_SIZE: u64 = 1024 * 1024;
const SYNTHETIC_CHUNK: &[u8] = &[0xab; 1024 * 1024];

/// QUIC flow-control profile for large Heddle object streams.
pub fn transport_config() -> QuicTransportConfig {
    let mut acknowledgements = AckFrequencyConfig::default();
    acknowledgements.ack_eliciting_threshold(50u32.into());
    QuicTransportConfig::builder()
        .ack_frequency_config(Some(acknowledgements))
        .build()
}

/// Write one request as one Iroh chunk and use the stream FIN as its delimiter.
pub async fn write_request<M: Message>(
    send: &mut SendStream,
    method: &str,
    message: &M,
) -> Result<()> {
    let frame = frame_request(method, &message.encode_to_vec())?;
    send.write_chunk(frame)
        .await
        .map_err(|error| TransportError::Iroh(error.to_string()))?;
    send.finish()
        .map_err(|error| TransportError::Iroh(error.to_string()))
}

/// Request an experiment-only synthetic response of exactly `len` bytes.
pub async fn write_wire_benchmark_request(send: &mut SendStream, len: u64) -> Result<()> {
    let frame = frame_request(METHOD_WIRE_BENCHMARK, &len.to_be_bytes())?;
    send.write_chunk(frame)
        .await
        .map_err(|error| TransportError::Iroh(error.to_string()))?;
    send.finish()
        .map_err(|error| TransportError::Iroh(error.to_string()))
}

/// Read an EOF-delimited request written by [`write_request`].
pub async fn read_request(recv: &mut RecvStream) -> Result<(Bytes, Bytes)> {
    let frame = recv
        .read_to_end(MAX_CONTROL_FRAME + MAX_METHOD_PATH + 2)
        .await
        .map_err(|error| TransportError::Iroh(error.to_string()))?;
    split_request_frame(frame)
}

fn frame_request(method: &str, payload: &[u8]) -> Result<Bytes> {
    let method = method.as_bytes();
    if method.is_empty() || !method.starts_with(b"/") || method.len() > MAX_METHOD_PATH {
        return Err(TransportError::InvalidFrame(
            "method path must start with '/' and fit the method-path limit".to_string(),
        ));
    }
    if payload.len() > MAX_CONTROL_FRAME {
        return Err(TransportError::InvalidFrame(format!(
            "control message is {} bytes; maximum is {MAX_CONTROL_FRAME}",
            payload.len()
        )));
    }
    let method_len = u16::try_from(method.len())
        .map_err(|_| TransportError::InvalidFrame("method path exceeds u16".to_string()))?;
    let mut frame = Vec::with_capacity(2 + method.len() + payload.len());
    frame.put_u16(method_len);
    frame.extend_from_slice(method);
    frame.extend_from_slice(payload);
    Ok(Bytes::from(frame))
}

fn split_request_frame(frame: Vec<u8>) -> Result<(Bytes, Bytes)> {
    if frame.len() < 2 {
        return Err(TransportError::InvalidFrame(
            "operation stream request has no method-path length".to_string(),
        ));
    }
    let method_len = u16::from_be_bytes([frame[0], frame[1]]) as usize;
    if method_len == 0 || method_len > MAX_METHOD_PATH || frame.len() < 2 + method_len {
        return Err(TransportError::InvalidFrame(format!(
            "operation stream declares invalid {method_len}-byte method path"
        )));
    }
    let frame = Bytes::from(frame);
    let method = frame.slice(2..2 + method_len);
    if !method.starts_with(b"/") || std::str::from_utf8(&method).is_err() {
        return Err(TransportError::InvalidFrame(
            "operation stream method path is not valid UTF-8 beginning with '/'".to_string(),
        ));
    }
    let payload = frame.slice(2 + method_len..);
    if payload.len() > MAX_CONTROL_FRAME {
        return Err(TransportError::InvalidFrame(format!(
            "control message is {} bytes; maximum is {MAX_CONTROL_FRAME}",
            payload.len()
        )));
    }
    Ok((method, payload))
}

/// Write one response and use the stream FIN as its delimiter.
pub async fn write_response<M: Message>(send: &mut SendStream, message: &M) -> Result<()> {
    let payload = message.encode_to_vec();
    if payload.len() > MAX_CONTROL_FRAME {
        return Err(TransportError::InvalidFrame(format!(
            "control message is {} bytes; maximum is {MAX_CONTROL_FRAME}",
            payload.len()
        )));
    }
    send.write_chunk(Bytes::from(payload))
        .await
        .map_err(|error| TransportError::Iroh(error.to_string()))?;
    send.finish()
        .map_err(|error| TransportError::Iroh(error.to_string()))
}

/// Decode one response delimited by the stream FIN.
pub async fn read_response<M: Message + Default>(recv: &mut RecvStream) -> Result<M> {
    let payload = recv
        .read_to_end(MAX_CONTROL_FRAME)
        .await
        .map_err(|error| TransportError::Iroh(error.to_string()))?;
    Ok(M::decode(payload.as_slice())?)
}

/// Send the fixed pull prelude and `PullReady` in a single Iroh chunk.
pub async fn write_pull_prelude<M: Message>(
    send: &mut SendStream,
    message: &M,
    pack_len: u64,
    index_len: u64,
) -> Result<()> {
    let payload_len = message.encoded_len();
    if payload_len > MAX_CONTROL_FRAME {
        return Err(TransportError::InvalidFrame(format!(
            "pull prelude is {} bytes; maximum is {MAX_CONTROL_FRAME}",
            payload_len
        )));
    }
    let payload_len_u32 = u32::try_from(payload_len)
        .map_err(|_| TransportError::InvalidFrame("pull prelude length exceeds u32".to_string()))?;
    let mut frame = Vec::with_capacity(20 + payload_len);
    frame.put_u32(payload_len_u32);
    frame.put_u64(pack_len);
    frame.put_u64(index_len);
    message.encode(&mut frame)?;
    send.write_chunk(Bytes::from(frame))
        .await
        .map_err(|error| TransportError::Iroh(error.to_string()))
}

/// Read the fixed pull prelude followed by its typed `PullReady` payload.
pub async fn read_pull_prelude<M: Message + Default>(
    recv: &mut RecvStream,
) -> Result<(M, u64, u64)> {
    let message_len = recv.read_u32().await? as usize;
    if message_len > MAX_CONTROL_FRAME {
        return Err(TransportError::InvalidFrame(format!(
            "pull prelude declares {message_len} bytes; maximum is {MAX_CONTROL_FRAME}"
        )));
    }
    let pack_len = recv.read_u64().await?;
    let index_len = recv.read_u64().await?;
    let mut payload = vec![0; message_len];
    AsyncReadExt::read_exact(recv, &mut payload).await?;
    Ok((M::decode(payload.as_slice())?, pack_len, index_len))
}

/// Send a file as owned Iroh chunks without the generic `AsyncWrite` copy path.
pub async fn write_file_body(send: &mut SendStream, path: &Path, len: u64) -> Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    let actual_len = file.metadata().await?.len();
    if actual_len != len {
        return Err(TransportError::InvalidFrame(format!(
            "pack file length changed before streaming: expected {len} bytes, found {actual_len}"
        )));
    }

    let mut sent = 0u64;
    while sent < len {
        let chunk_len = usize::try_from((len - sent).min(FILE_SEND_CHUNK_SIZE)).map_err(|_| {
            TransportError::InvalidFrame("file send chunk length exceeds usize".to_string())
        })?;
        let mut chunk = vec![0; chunk_len];
        AsyncReadExt::read_exact(&mut file, &mut chunk).await?;
        send.write_chunk(Bytes::from(chunk))
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
        sent += chunk_len as u64;
    }
    Ok(())
}

/// Send a generated body without storage or encoding work in the measured path.
pub async fn write_synthetic_body(send: &mut SendStream, len: u64) -> Result<()> {
    let chunk = Bytes::from_static(SYNTHETIC_CHUNK);
    let full_chunks = len / SYNTHETIC_CHUNK.len() as u64;
    let remaining = (len % SYNTHETIC_CHUNK.len() as u64) as usize;

    for _ in 0..full_chunks {
        send.write_chunk(chunk.clone())
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
    }
    if remaining > 0 {
        send.write_chunk(chunk.slice(..remaining))
            .await
            .map_err(|error| TransportError::Iroh(error.to_string()))?;
    }
    send.finish()
        .map_err(|error| TransportError::Iroh(error.to_string()))
}

/// Receive one known-length raw body from an operation stream into the spool.
pub async fn read_file_body(
    recv: &mut RecvStream,
    len: u64,
    maximum: u64,
    spool: &mut PackChunkSpool,
    is_index: bool,
) -> Result<u64> {
    if len > maximum {
        return Err(TransportError::InvalidFrame(format!(
            "peer declared {len}-byte data body; maximum is {maximum}"
        )));
    }
    if len == 0 {
        return Err(TransportError::InvalidFrame(
            "native pack data body cannot be empty".to_string(),
        ));
    }

    let mut buffer = vec![0u8; DATA_CHUNK_SIZE];
    let mut offset = 0u64;
    let mut chunk_index = 0u32;
    while offset < len {
        let remaining = len - offset;
        let chunk_len = remaining.min(DATA_CHUNK_SIZE as u64) as usize;
        AsyncReadExt::read_exact(recv, &mut buffer[..chunk_len]).await?;
        let next_offset = offset + chunk_len as u64;
        let is_final = next_offset == len;
        spool.receive_chunk(
            is_index,
            offset,
            chunk_index,
            is_final,
            &buffer[..chunk_len],
            is_final,
        )?;
        offset = next_offset;
        chunk_index = chunk_index.checked_add(1).ok_or_else(|| {
            TransportError::InvalidFrame("data stream chunk index overflow".to_string())
        })?;
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frame_preserves_the_contract_method_path() {
        let frame = frame_request(METHOD_LIST_REFS, b"request").unwrap();
        let (method, payload) = split_request_frame(frame.to_vec()).unwrap();

        assert_eq!(method, METHOD_LIST_REFS.as_bytes());
        assert_eq!(payload, &b"request"[..]);
    }

    #[test]
    fn request_frame_rejects_a_non_contract_method_name() {
        let error = frame_request("ListRefs", b"").unwrap_err();
        assert!(matches!(error, TransportError::InvalidFrame(_)));
    }
}
