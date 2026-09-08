// SPDX-License-Identifier: Apache-2.0
//! One reliable Iroh bidirectional stream per RPC, with bounded framing and
//! cancellation-safe readers. The caller owns connection and credential setup.
use std::time::Duration;

use ::iroh::endpoint::{Connection, RecvStream, SendStream};
use api::{
    framing,
    heddle::api::v1alpha1::CallFailure,
    v2::{
        MethodDescriptor,
        client::{MessageReader, MessageWriter, RpcTransport},
    },
};
use prost::Message;

use super::{Authorize, Error};

fn io(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
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
        let mut writer = Writer::new(send, self.frame_limit, self.progress_timeout);
        let reader = Reader::for_method(recv, self.frame_limit, self.progress_timeout, method);
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
    opening: bool,
    live: bool,
    frame_deadline: Option<tokio::time::Instant>,
}

impl Reader {
    pub(crate) fn new(recv: RecvStream, frame_limit: usize, timeout: Duration) -> Self {
        Self {
            recv,
            frame_limit,
            timeout,
            done: false,
            buffer: Vec::with_capacity(5),
            opening: true,
            live: false,
            frame_deadline: None,
        }
    }

    pub(crate) fn for_method(
        recv: RecvStream,
        frame_limit: usize,
        timeout: Duration,
        method: &MethodDescriptor,
    ) -> Self {
        let mut reader = Self::new(recv, frame_limit, timeout);
        reader.live = method.live_stream;
        reader
    }

    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if (!self.live || self.opening) && self.frame_deadline.is_none() {
            self.frame_deadline = Some(tokio::time::Instant::now() + self.timeout);
        }
        loop {
            // Keep a started frame's deadline on Reader, just like its bytes.
            // Cancellation and a later next() cannot restart that deadline.
            if self
                .frame_deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
            {
                return Err(Error::Timeout);
            }
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
                self.opening = false;
                self.frame_deadline = None;
                return Ok(Some(frame));
            }
            // Partial framing lives on Reader, so selecting another future while
            // next() waits cannot discard a consumed header or body prefix.
            let mut chunk = [0; 8192];
            let size = (needed - self.buffer.len()).min(chunk.len());
            let read = match self.frame_deadline {
                Some(deadline) => {
                    tokio::time::timeout_at(deadline, self.recv.read(&mut chunk[..size]))
                        .await
                        .map_err(|_| Error::Timeout)?
                        .map_err(io)?
                }
                // A live stream may have no new records for hours. Iroh owns
                // connection liveness; callers own cancellation/overall waits.
                None => self.recv.read(&mut chunk[..size]).await.map_err(io)?,
            };
            match read {
                Some(n) => {
                    if self.frame_deadline.is_none() {
                        self.frame_deadline = Some(tokio::time::Instant::now() + self.timeout);
                    }
                    self.buffer.extend_from_slice(&chunk[..n]);
                }
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
        let result = self.read_frame().await;
        if result.is_err() {
            self.cancel();
        }
        result
    }
    fn cancel(&mut self) {
        if !self.done {
            let _ = self.recv.stop(0u32.into());
            self.done = true;
            self.buffer = Vec::new();
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
    pub(crate) fn new(send: SendStream, frame_limit: usize, timeout: Duration) -> Self {
        Self {
            send: Some(send),
            frame_limit,
            timeout,
            poisoned: false,
        }
    }

    #[cfg(feature = "native")]
    pub(crate) async fn fail(&mut self, failure: &CallFailure) -> Result<(), Error> {
        self.write(&framing::encode_stream_failure(failure)?)
            .await?;
        self.finish().await
    }

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
#[path = "../transport_tests.rs"]
mod tests;
