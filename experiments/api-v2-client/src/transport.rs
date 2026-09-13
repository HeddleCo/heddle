// SPDX-License-Identifier: Apache-2.0
//! One reliable Iroh bidirectional stream per RPC. No route-name inventory,
//! background receive queue, implicit retries, or v1 RPC dispatch.
use std::{future::Future, time::Duration};

use api::{
    framing,
    heddle::api::v1alpha1::{CallContext, CallFailure},
    v2::{
        MethodDescriptor,
        client::{MessageReader, MessageWriter, RpcTransport},
    },
};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use prost::Message;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Iroh: {0}")]
    Io(String),
    #[error("RPC made no progress before its timeout")]
    Timeout,
    #[error("invalid v2 transport: {0}")]
    Protocol(&'static str),
    #[error("RPC failed: {0:?}")]
    Remote(RemoteFailure),
    #[error(transparent)]
    Framing(#[from] framing::FrameError),
    #[error(transparent)]
    Metadata(#[from] api::RequestMetadataError),
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
}

/// Keep large optional challenge/conflict details in their wire representation
/// until an application needs them. Common error handling needs only code and
/// message; every original typed detail remains available without heap boxing.
#[derive(Debug)]
pub struct RemoteFailure {
    pub code: i32,
    pub message: String,
    detail: Option<Vec<u8>>,
}
impl RemoteFailure {
    pub fn detail(
        &self,
    ) -> Result<Option<api::heddle::api::v1alpha1::ErrorDetail>, prost::DecodeError> {
        self.detail
            .as_deref()
            .map(prost::Message::decode)
            .transpose()
    }
}
impl From<CallFailure> for RemoteFailure {
    fn from(failure: CallFailure) -> Self {
        Self {
            code: failure.code,
            message: failure.message,
            detail: failure.error.map(|e| e.encode_to_vec()),
        }
    }
}

fn io(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}

/// Existing Heddle signer/broker integration plugs in here. Sign exactly the
/// supplied method and encoded body; carry the owner's Biscuit and attachment.
/// Shared CallContext/signing formats are retained data, not old RPC methods.
pub trait Authorize: Send + Sync {
    fn context(
        &self,
        method: &'static MethodDescriptor,
        body: &[u8],
    ) -> impl Future<Output = Result<CallContext, Error>> + Send;
}

pub struct IrohTransport<A> {
    connection: Connection,
    authorize: A,
    frame_limit: usize,
    progress_timeout: Duration,
}

impl<A: Authorize> IrohTransport<A> {
    pub fn new(
        connection: Connection,
        authorize: A,
        frame_limit: usize,
        progress_timeout: Duration,
    ) -> Result<Self, Error> {
        if frame_limit == 0 || frame_limit > framing::MAX_CONTROL_BODY || progress_timeout.is_zero()
        {
            return Err(Error::Protocol("invalid local transport limits"));
        }
        Ok(Self {
            connection,
            authorize,
            frame_limit,
            progress_timeout,
        })
    }

    async fn open(
        &self,
        method: &'static MethodDescriptor,
        body: &[u8],
        exchange: bool,
    ) -> Result<(Writer, Reader), Error> {
        if body.len() > self.frame_limit {
            return Err(Error::Protocol("request exceeds frame budget"));
        }
        let mut context = self.authorize.context(method, body).await?;
        context.client_operation_id = method.client_operation_id(body)?.unwrap_or_default().into();
        let (send, recv) = tokio::time::timeout(self.progress_timeout, self.connection.open_bi())
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(io)?;
        let mut writer = Writer {
            send: Some(send),
            frame_limit: self.frame_limit,
            timeout: self.progress_timeout,
            poisoned: false,
        };
        let reader = Reader {
            recv,
            frame_limit: self.frame_limit,
            timeout: self.progress_timeout,
            done: false,
            buffer: Vec::with_capacity(5),
        };
        let frame = if exchange {
            framing::encode_request_prelude(method.path, &context)?
        } else {
            framing::encode_request_frame(method.path, &context, body)?
        };
        writer.write(&frame).await?;
        if exchange {
            writer.send(body.to_vec()).await?;
        } else {
            writer.finish().await?;
        }
        Ok((writer, reader))
    }
}

impl<A: Authorize> RpcTransport for IrohTransport<A> {
    type Error = Error;
    type Reader = Reader;
    type Writer = Writer;

    async fn unary(
        &self,
        method: &'static MethodDescriptor,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        let (_, mut reader) = self.open(method, &request, false).await?;
        let frame = tokio::time::timeout(
            reader.timeout,
            reader.recv.read_to_end(reader.frame_limit + 1),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(io)?;
        reader.done = true;
        match framing::decode_response_frame(&frame)? {
            framing::ResponseFrame::Success(bytes) => Ok(bytes.to_vec()),
            framing::ResponseFrame::Failure(failure) => Err(Error::Remote(failure.into())),
        }
    }

    async fn observe(
        &self,
        method: &'static MethodDescriptor,
        request: Vec<u8>,
    ) -> Result<Reader, Error> {
        Ok(self.open(method, &request, false).await?.1)
    }

    async fn exchange(
        &self,
        method: &'static MethodDescriptor,
        opening: Vec<u8>,
    ) -> Result<(Writer, Reader), Error> {
        self.open(method, &opening, true).await
    }
}

pub struct Reader {
    recv: RecvStream,
    frame_limit: usize,
    timeout: Duration,
    done: bool,
    buffer: Vec<u8>,
}

impl Reader {
    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            let needed = if self.buffer.len() < 5 {
                5
            } else {
                let header = &self.buffer;
                if header[0] > 1 {
                    return Err(Error::Protocol("unexpected stream frame kind"));
                }
                let length =
                    u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
                // Check before allocating or reading any body bytes.
                if length > self.frame_limit {
                    return Err(Error::Protocol("response exceeds frame budget"));
                }
                length + 5
            };
            if self.buffer.len() == needed {
                let mut frame = std::mem::take(&mut self.buffer);
                let kind = frame[0];
                frame.drain(..5);
                if kind == 1 {
                    return Err(Error::Remote(CallFailure::decode(frame.as_slice())?.into()));
                }
                return Ok(Some(frame));
            }
            // Partial framing lives on Reader, so selecting another future while
            // next() waits cannot discard a consumed header or body prefix.
            let mut chunk = [0; 8192];
            let size = (needed - self.buffer.len()).min(chunk.len());
            match self.recv.read(&mut chunk[..size]).await.map_err(io)? {
                Some(n) => self.buffer.extend_from_slice(&chunk[..n]),
                None if self.buffer.is_empty() => {
                    self.done = true;
                    return Ok(None);
                }
                None => return Err(Error::Protocol("FIN within a stream frame")),
            }
        }
    }
}

impl MessageReader for Reader {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if self.done {
            return Ok(None);
        }
        let result = tokio::time::timeout(self.timeout, self.read_frame()).await;
        let result = result.map_err(|_| Error::Timeout).and_then(|r| r);
        if result.is_err() {
            self.cancel();
        }
        result
    }
    fn cancel(&mut self) {
        if !self.done {
            let _ = self.recv.stop(0u32.into());
            self.done = true;
        }
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub struct Writer {
    send: Option<SendStream>,
    frame_limit: usize,
    timeout: Duration,
    poisoned: bool,
}
impl Writer {
    async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let send = self
            .send
            .as_mut()
            .ok_or(Error::Protocol("request stream is closed"))?;
        if self.poisoned {
            return Err(Error::Protocol(
                "previous write interrupted; reopen the RPC",
            ));
        }
        self.poisoned = true;
        tokio::time::timeout(self.timeout, send.write_all(bytes))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(io)?;
        self.poisoned = false;
        Ok(())
    }
}
impl MessageWriter for Writer {
    type Error = Error;
    async fn send(&mut self, message: Vec<u8>) -> Result<(), Error> {
        if message.len() > self.frame_limit {
            return Err(Error::Protocol("request exceeds frame budget"));
        }
        self.write(&framing::encode_stream_message(&message)?).await
    }
    async fn finish(&mut self) -> Result<(), Error> {
        if self.poisoned {
            self.abort();
            return Err(Error::Protocol("cannot finish an interrupted request"));
        }
        if let Some(mut send) = self.send.take() {
            send.finish().map_err(io)?;
        }
        Ok(())
    }
    fn abort(&mut self) {
        if let Some(mut send) = self.send.take() {
            let _ = send.reset(0u32.into());
        }
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
