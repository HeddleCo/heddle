use std::{marker::PhantomData, sync::Arc};

use api::{
    StreamingShape,
    framing::{
        MAX_CONTROL_BODY, ResponseFrame, StreamFrame, decode_response_frame, decode_stream_frame,
        encode_request_frame, encode_request_prelude, encode_stream_message,
    },
    heddle::api::v1alpha1::CallContext,
    method_descriptor,
};
use bytes::Bytes;
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
    })
}

/// Decoded server-streaming response over one operation stream.
pub struct ServerStream<Response> {
    _connection: Arc<HostedConnection>,
    recv: iroh::endpoint::RecvStream,
    buffered: Vec<u8>,
    response: PhantomData<Response>,
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
        }
    }

    pub async fn next(&mut self) -> Result<Option<Response>> {
        loop {
            if let Some((frame, consumed)) =
                decode_stream_frame(&self.buffered).map_err(HostedError::framing)?
            {
                let response = match frame {
                    StreamFrame::Message(body) => Response::decode(body)?,
                    StreamFrame::Failure(failure) => return Err(failure.into()),
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
                None if self.buffered.is_empty() => return Ok(None),
                None => {
                    return Err(HostedError::Framing(
                        "server stream ended with a truncated frame".to_string(),
                    ));
                }
            }
        }
    }

    pub fn cancel(&mut self) -> Result<()> {
        self.recv.stop(1u32.into()).map_err(HostedError::transport)
    }
}

/// Bidirectional operation stream with typed protobuf messages in both directions.
pub struct BidirectionalStream<Request, Response> {
    send: Option<iroh::endpoint::SendStream>,
    responses: ServerStream<Response>,
    request: PhantomData<Request>,
}

impl<Request, Response> BidirectionalStream<Request, Response>
where
    Request: Message,
    Response: Message + Default,
{
    pub async fn send(&mut self, request: &Request) -> Result<()> {
        let frame =
            encode_stream_message(&request.encode_to_vec()).map_err(HostedError::framing)?;
        self.send
            .as_mut()
            .ok_or_else(|| HostedError::Framing("request stream is finished".to_string()))?
            .write_chunk(Bytes::from(frame))
            .await
            .map_err(HostedError::transport)
    }

    pub fn finish_requests(&mut self) -> Result<()> {
        if let Some(mut send) = self.send.take() {
            send.finish().map_err(HostedError::transport)?;
        }
        Ok(())
    }

    pub async fn next(&mut self) -> Result<Option<Response>> {
        self.responses.next().await
    }

    pub fn cancel(&mut self) -> Result<()> {
        if let Some(mut send) = self.send.take() {
            send.reset(1u32.into()).map_err(HostedError::transport)?;
        }
        self.responses.cancel()
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
