use std::{marker::PhantomData, sync::Arc};

use api::{
    StreamingShape,
    framing::{
        MAX_CONTROL_BODY, ResponseFrame, StreamFrame, decode_response_frame, decode_stream_frame,
        encode_request_frame, encode_request_prelude, encode_stream_message_into,
    },
    heddle::api::v1alpha1::CallContext,
    method_descriptor,
};
use bytes::{Bytes, BytesMut};
use prost::Message;

use super::{HostedConnection, HostedError, Result};

pub(super) async fn unary<Request, Response>(
    connection: &HostedConnection,
    method: &str,
    context: &CallContext,
    request: &Request,
) -> Result<Response>
where
    Request: Message,
    Response: Message + Default,
{
    require_shape(method, StreamingShape::Unary)?;
    unary_encoded(connection, method, context, &request.encode_to_vec()).await
}

pub(super) async fn unary_encoded<Response>(
    connection: &HostedConnection,
    method: &str,
    context: &CallContext,
    encoded_request: &[u8],
) -> Result<Response>
where
    Response: Message + Default,
{
    require_shape(method, StreamingShape::Unary)?;
    let frame =
        encode_request_frame(method, context, encoded_request).map_err(HostedError::framing)?;
    let (mut send, mut recv) = connection
        .connection
        .open_bi()
        .await
        .map_err(HostedError::transport)?;
    send.write_chunk(Bytes::from(frame))
        .await
        .map_err(HostedError::transport)?;
    send.finish().map_err(HostedError::transport)?;
    let response = recv
        .read_to_end(MAX_CONTROL_BODY + 1)
        .await
        .map_err(HostedError::transport)?;
    match decode_response_frame(&response).map_err(HostedError::framing)? {
        ResponseFrame::Success(body) => Response::decode(body).map_err(HostedError::from),
        ResponseFrame::Failure(failure) => Err(failure.into()),
    }
}

pub(super) async fn server_stream<Request, Response>(
    connection: Arc<HostedConnection>,
    method: &str,
    context: &CallContext,
    request: &Request,
) -> Result<ServerStream<Response>>
where
    Request: Message,
    Response: Message + Default,
{
    require_shape(method, StreamingShape::ServerStreaming)?;
    let mut frame = encode_request_prelude(method, context).map_err(HostedError::framing)?;
    frame.extend_from_slice(&request.encode_to_vec());
    let (mut send, recv) = connection
        .connection
        .open_bi()
        .await
        .map_err(HostedError::transport)?;
    send.write_chunk(Bytes::from(frame))
        .await
        .map_err(HostedError::transport)?;
    send.finish().map_err(HostedError::transport)?;
    Ok(ServerStream::new(connection, recv))
}

pub(super) async fn bidirectional<Request, Response>(
    connection: Arc<HostedConnection>,
    method: &str,
    context: &CallContext,
) -> Result<BidirectionalStream<Request, Response>>
where
    Request: Message,
    Response: Message + Default,
{
    require_shape(method, StreamingShape::Bidirectional)?;
    let prelude = encode_request_prelude(method, context).map_err(HostedError::framing)?;
    let (mut send, recv) = connection
        .connection
        .open_bi()
        .await
        .map_err(HostedError::transport)?;
    send.write_chunk(Bytes::from(prelude))
        .await
        .map_err(HostedError::transport)?;
    Ok(BidirectionalStream {
        send: Some(send),
        responses: ServerStream::new(connection, recv),
        request: PhantomData,
        raw_remaining: 0,
        control: BytesMut::new(),
    })
}

/// Decoded server-streaming response over one operation stream.
pub struct ServerStream<Response> {
    _connection: Arc<HostedConnection>,
    recv: iroh::endpoint::RecvStream,
    buffered: Vec<u8>,
    response: PhantomData<Response>,
    raw_remaining: u64,
    finished: bool,
}

#[derive(Debug)]
pub enum ServerStreamItem<Response> {
    Message(Response),
    RawBody { length: u64 },
}

impl<Response> ServerStream<Response>
where
    Response: Message + Default,
{
    fn new(connection: Arc<HostedConnection>, recv: iroh::endpoint::RecvStream) -> Self {
        Self {
            _connection: connection,
            recv,
            buffered: Vec::new(),
            response: PhantomData,
            raw_remaining: 0,
            finished: false,
        }
    }

    pub async fn next(&mut self) -> Result<Option<Response>> {
        match self.next_item().await? {
            Some(ServerStreamItem::Message(response)) => Ok(Some(response)),
            Some(ServerStreamItem::RawBody { length }) => Err(HostedError::Framing(format!(
                "unexpected {length}-byte raw body on typed stream"
            ))),
            None => Ok(None),
        }
    }

    pub async fn next_item(&mut self) -> Result<Option<ServerStreamItem<Response>>> {
        if self.raw_remaining != 0 {
            return Err(HostedError::Framing(
                "raw body must be consumed before the next stream item".to_string(),
            ));
        }
        loop {
            if let Some((frame, consumed)) =
                decode_stream_frame(&self.buffered).map_err(HostedError::framing)?
            {
                let response = match frame {
                    StreamFrame::Message(body) => {
                        ServerStreamItem::Message(Response::decode(body)?)
                    }
                    StreamFrame::Failure(failure) => return Err(failure.into()),
                    StreamFrame::RawBody { length } => {
                        self.raw_remaining = length;
                        ServerStreamItem::RawBody { length }
                    }
                };
                self.buffered.drain(..consumed);
                return Ok(Some(response));
            }
            match self
                .recv
                .read_chunk(MAX_CONTROL_BODY + 5)
                .await
                .map_err(HostedError::transport)?
            {
                Some(chunk) => self.buffered.extend_from_slice(&chunk),
                None if self.buffered.is_empty() => {
                    self.finished = true;
                    return Ok(None);
                }
                None => {
                    return Err(HostedError::Framing(
                        "server stream ended with a truncated frame".to_string(),
                    ));
                }
            }
        }
    }

    pub async fn read_raw_chunk(&mut self, maximum: usize) -> Result<Option<Bytes>> {
        if self.raw_remaining == 0 {
            return Ok(None);
        }
        let maximum = maximum.max(1);
        if !self.buffered.is_empty() {
            let length = self
                .buffered
                .len()
                .min(maximum)
                .min(usize::try_from(self.raw_remaining).unwrap_or(usize::MAX));
            let chunk = Bytes::copy_from_slice(&self.buffered[..length]);
            self.buffered.drain(..length);
            self.raw_remaining -= length as u64;
            return Ok(Some(chunk));
        }
        let Some(chunk) = self
            .recv
            .read_chunk(maximum)
            .await
            .map_err(HostedError::transport)?
        else {
            return Err(HostedError::Framing(
                "stream ended within a declared raw body".to_string(),
            ));
        };
        let accepted = chunk
            .len()
            .min(usize::try_from(self.raw_remaining).unwrap_or(usize::MAX));
        if accepted < chunk.len() {
            self.buffered.extend_from_slice(&chunk[accepted..]);
        }
        self.raw_remaining -= accepted as u64;
        Ok(Some(chunk.slice(..accepted)))
    }

    pub fn cancel(&mut self) -> Result<()> {
        if !self.finished {
            self.recv
                .stop(1u32.into())
                .map_err(HostedError::transport)?;
            self.finished = true;
        }
        Ok(())
    }
}

impl<Response> Drop for ServerStream<Response> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.recv.stop(1u32.into());
        }
    }
}

/// Bidirectional operation stream with typed protobuf messages in both directions.
pub struct BidirectionalStream<Request, Response> {
    send: Option<iroh::endpoint::SendStream>,
    responses: ServerStream<Response>,
    request: PhantomData<Request>,
    raw_remaining: u64,
    control: BytesMut,
}

/// Request half of a bidirectional operation stream.
pub struct BidirectionalRequestStream<Request> {
    send: Option<iroh::endpoint::SendStream>,
    request: PhantomData<Request>,
    raw_remaining: u64,
    control: BytesMut,
}

impl<Request, Response> BidirectionalStream<Request, Response>
where
    Request: Message,
    Response: Message + Default,
{
    pub fn split(self) -> (BidirectionalRequestStream<Request>, ServerStream<Response>) {
        (
            BidirectionalRequestStream {
                send: self.send,
                request: PhantomData,
                raw_remaining: self.raw_remaining,
                control: self.control,
            },
            self.responses,
        )
    }

    pub async fn send(&mut self, request: &Request) -> Result<()> {
        if self.raw_remaining != 0 {
            return Err(HostedError::Framing(
                "raw body must finish before the next request message".to_string(),
            ));
        }
        encode_stream_message_into(&mut self.control, &request.encode_to_vec())
            .map_err(HostedError::framing)?;
        self.send
            .as_mut()
            .ok_or_else(|| HostedError::Framing("request stream is finished".to_string()))?
            .write_all(&self.control)
            .await
            .map_err(HostedError::transport)
    }

    pub fn finish_requests(&mut self) -> Result<()> {
        if self.raw_remaining != 0 {
            return Err(HostedError::Framing(
                "cannot finish a stream within a declared raw body".to_string(),
            ));
        }
        if let Some(mut send) = self.send.take() {
            send.finish().map_err(HostedError::transport)?;
        }
        Ok(())
    }

    pub async fn next(&mut self) -> Result<Option<Response>> {
        self.responses.next().await
    }

    pub async fn next_item(&mut self) -> Result<Option<ServerStreamItem<Response>>> {
        self.responses.next_item().await
    }

    pub async fn read_raw_chunk(&mut self, maximum: usize) -> Result<Option<Bytes>> {
        self.responses.read_raw_chunk(maximum).await
    }

    pub fn cancel(&mut self) -> Result<()> {
        if let Some(mut send) = self.send.take() {
            send.reset(1u32.into()).map_err(HostedError::transport)?;
        }
        self.responses.cancel()
    }
}

impl<Request> BidirectionalRequestStream<Request>
where
    Request: Message,
{
    pub async fn send(&mut self, request: &Request) -> Result<()> {
        if self.raw_remaining != 0 {
            return Err(HostedError::Framing(
                "raw body must finish before the next request message".to_string(),
            ));
        }
        encode_stream_message_into(&mut self.control, &request.encode_to_vec())
            .map_err(HostedError::framing)?;
        self.send
            .as_mut()
            .ok_or_else(|| HostedError::Framing("request stream is finished".to_string()))?
            .write_all(&self.control)
            .await
            .map_err(HostedError::transport)
    }

    pub async fn begin_raw(&mut self, length: u64) -> Result<()> {
        if self.raw_remaining != 0 {
            return Err(HostedError::Framing(
                "a raw request body is already active".to_string(),
            ));
        }
        api::framing::encode_stream_raw_body_into(&mut self.control, length)
            .map_err(HostedError::framing)?;
        self.send
            .as_mut()
            .ok_or_else(|| HostedError::Framing("request stream is finished".to_string()))?
            .write_all(&self.control)
            .await
            .map_err(HostedError::transport)?;
        self.raw_remaining = length;
        Ok(())
    }

    pub async fn send_raw_chunk(&mut self, chunk: Bytes) -> Result<()> {
        if chunk.is_empty() || chunk.len() as u64 > self.raw_remaining {
            return Err(HostedError::Framing(
                "raw request chunk exceeds the declared body".to_string(),
            ));
        }
        let length = chunk.len() as u64;
        self.send
            .as_mut()
            .ok_or_else(|| HostedError::Framing("request stream is finished".to_string()))?
            .write_chunk(chunk)
            .await
            .map_err(HostedError::transport)?;
        self.raw_remaining -= length;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.raw_remaining != 0 {
            return Err(HostedError::Framing(
                "cannot finish a stream within a declared raw body".to_string(),
            ));
        }
        if let Some(mut send) = self.send.take() {
            send.finish().map_err(HostedError::transport)?;
        }
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<()> {
        if let Some(mut send) = self.send.take() {
            send.reset(1u32.into()).map_err(HostedError::transport)?;
        }
        Ok(())
    }
}

fn require_shape(method: &str, expected: StreamingShape) -> Result<()> {
    let descriptor = method_descriptor(method)
        .ok_or_else(|| HostedError::Framing(format!("unknown hosted method {method}")))?;
    if descriptor.streaming != expected {
        return Err(HostedError::Framing(format!(
            "hosted method {method} has {:?} shape, expected {expected:?}",
            descriptor.streaming
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, sync::Arc, time::Duration};

    use api::heddle::api::v1alpha1::PushServerFrame;
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use tokio::sync::oneshot;

    use super::{super::HostedConnection, ServerStream};

    #[tokio::test]
    async fn dropping_an_open_server_stream_stops_the_remote_response_sender() {
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let server_addr = server.addr();
        let (response_started_tx, response_started_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.expect("incoming connection");
            let connection = incoming.await.expect("accept connection");
            let (mut send, mut recv) = connection.accept_bi().await.expect("accept stream");
            recv.read_chunk(1)
                .await
                .expect("read request byte")
                .expect("request stream remains open");
            send.write_all(b"pending response").await.unwrap();
            response_started_tx.send(()).unwrap();
            let stop_code = tokio::time::timeout(Duration::from_secs(2), send.stopped())
                .await
                .expect("dropping ServerStream must signal STOP_SENDING")
                .expect("remote response sender observes the stop code");
            assert_eq!(stop_code, Some(1u32.into()));
        });

        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let connection = HostedConnection::connect(client, server_addr)
            .await
            .unwrap();
        let (mut send, recv) = connection.connection.open_bi().await.unwrap();
        send.write_all(b"x").await.unwrap();
        send.finish().unwrap();
        response_started_rx.await.unwrap();
        let response = ServerStream::<PushServerFrame>::new(Arc::clone(&connection), recv);
        drop(response);

        server_task.await.unwrap();
    }
}
