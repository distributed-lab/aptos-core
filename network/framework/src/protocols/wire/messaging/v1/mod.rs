// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This module defines the AptosNet v1 message types, how they are
//! serialized/deserialized, and provides a `Sink` and `Stream` implementation
//! for sending `NetworkMessage`s over an abstract IO object (presumably a socket).
//!
//! The [AptosNet Specification](https://github.com/aptos-labs/aptos-core/blob/main/documentation/specifications/network/messaging-v1.md)
//! describes in greater detail how these messages are sent and received
//! over-the-wire.

use crate::protocols::{stream::StreamMessage, wire::handshake::v1::ProtocolId};
use bytes::Bytes;
use futures::{
    io::{AsyncRead, AsyncWrite},
    sink::Sink,
    stream::Stream,
};
use pin_project::pin_project;
#[cfg(any(test, feature = "fuzzing"))]
use proptest_derive::Arbitrary;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio_util::{
    codec::{FramedRead, FramedWrite, LengthDelimitedCodec},
    compat::{Compat, FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt},
};

pub mod metadata;
#[cfg(test)]
mod test;

/// The most primitive message type sent on the network
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub enum NetworkMessage {
    Error(ErrorCode),
    RpcRequest(RpcRequest),
    RpcResponse(RpcResponse),
    DirectSendMsg(DirectSendMsg),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub enum MultiplexMessage {
    Message(NetworkMessage),
    Stream(StreamMessage),
}

impl NetworkMessage {
    /// The size of the raw data excluding the headers
    pub fn data_length(&self) -> u64 {
        match self {
            NetworkMessage::Error(_) => 0,
            NetworkMessage::RpcRequest(request) => request.data_length(),
            NetworkMessage::RpcResponse(response) => response.raw_response.len() as u64,
            NetworkMessage::DirectSendMsg(message) => message.data_length(),
        }
    }

    /// Creates a direct send message with the default priority
    pub fn new_direct_send(protocol_id: ProtocolId, raw_msg: Vec<u8>) -> Self {
        let direct_send_message = DirectSendMsg::new(protocol_id, Priority::default(), raw_msg);
        NetworkMessage::DirectSendMsg(direct_send_message)
    }

    /// Creates an RPC response message with the default priority
    pub fn new_rpc_response(request_id: RequestId, raw_response: Vec<u8>) -> Self {
        let rpc_response = RpcResponse::new(request_id, Priority::default(), raw_response);
        NetworkMessage::RpcResponse(rpc_response)
    }

    /// Creates an RPC response message for testing.
    /// Note: this cannot be marked as `#[cfg(test)]` because of several non-wrapped test utils.
    pub fn rpc_response_for_testing(raw_response: Vec<u8>) -> Self {
        Self::new_rpc_response(0, raw_response)
    }

    /// Creates an RPC request message with the default priority
    pub fn new_rpc_request(
        protocol_id: ProtocolId,
        request_id: RequestId,
        raw_request: Vec<u8>,
    ) -> Self {
        let rpc_request =
            RpcRequest::new(protocol_id, request_id, Priority::default(), raw_request);
        NetworkMessage::RpcRequest(rpc_request)
    }

    /// Creates an RPC request message for testing.
    /// Note: this cannot be marked as `#[cfg(test)]` because of several non-wrapped test utils.
    pub fn rpc_request_for_testing(protocol_id: ProtocolId, raw_request: Vec<u8>) -> Self {
        Self::new_rpc_request(protocol_id, 0, raw_request)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub enum ErrorCode {
    /// Failed to parse NetworkMessage when interpreting according to provided protocol version.
    ParsingError(ParsingErrorType),
    /// A message was received for a protocol that is not supported over this connection.
    NotSupported(NotSupportedType),
}

impl ErrorCode {
    pub fn parsing_error(message: u8, protocol: u8) -> Self {
        ErrorCode::ParsingError(ParsingErrorType { message, protocol })
    }
}

/// Flags an invalid network message with as much header information as possible. This is a message
/// that this peer cannot even parse its header information.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct ParsingErrorType {
    message: u8,
    protocol: u8,
}

/// Flags an unsupported network message.  This is a message that a peer can parse its header
/// information but does not have a handler.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub enum NotSupportedType {
    RpcRequest(ProtocolId),
    DirectSendMsg(ProtocolId),
}

/// Create alias RequestId for `u32`.
pub type RequestId = u32;

/// Create alias Priority for u8.
pub type Priority = u8;

pub trait IncomingRequest {
    fn protocol_id(&self) -> crate::ProtocolId;
    fn data(&self) -> &Vec<u8>;

    /// Returns the length of the data in the request
    fn data_length(&self) -> u64 {
        self.data().len() as u64
    }

    /// Converts the `SerializedMessage` into its deserialized version of `TMessage` based on the
    /// `ProtocolId`.  See: [`crate::ProtocolId::from_bytes`]
    fn to_message<TMessage: DeserializeOwned>(&self) -> anyhow::Result<TMessage> {
        self.protocol_id().from_bytes(self.data())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct RpcRequest {
    /// `protocol_id` is a variant of the ProtocolId enum.
    protocol_id: ProtocolId,
    /// RequestId for the RPC Request.
    request_id: RequestId,
    /// Request priority in the range 0..=255.
    priority: Priority,
    /// Request payload. This will be parsed by the application-level handler.
    #[serde(with = "serde_bytes")]
    raw_request: Vec<u8>,
}

impl RpcRequest {
    pub fn new(
        protocol_id: ProtocolId,
        request_id: RequestId,
        priority: Priority,
        raw_request: Vec<u8>,
    ) -> Self {
        Self {
            protocol_id,
            request_id,
            priority,
            raw_request,
        }
    }

    /// Returns a mutable reference to the raw data of the RPC request
    pub fn data_mut(&mut self) -> &mut Vec<u8> {
        &mut self.raw_request
    }

    /// Returns the priority of the RPC request
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// Returns the ID of the RPC request
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }
}

impl IncomingRequest for RpcRequest {
    fn protocol_id(&self) -> crate::ProtocolId {
        self.protocol_id
    }

    fn data(&self) -> &Vec<u8> {
        &self.raw_request
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct RpcResponse {
    /// RequestId for corresponding request. This is copied as is from the RpcRequest.
    pub request_id: RequestId,
    /// Response priority in the range 0..=255. This will likely be same as the priority of
    /// corresponding request.
    pub priority: Priority,
    /// Response payload.
    #[serde(with = "serde_bytes")]
    pub raw_response: Vec<u8>,
}

impl RpcResponse {
    pub fn new(request_id: RequestId, priority: Priority, raw_response: Vec<u8>) -> Self {
        Self {
            request_id,
            priority,
            raw_response,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct DirectSendMsg {
    /// `protocol_id` is a variant of the ProtocolId enum.
    pub protocol_id: ProtocolId,
    /// Message priority in the range 0..=255.
    pub priority: Priority,
    /// Message payload.
    #[serde(with = "serde_bytes")]
    pub raw_msg: Vec<u8>,
}

impl DirectSendMsg {
    pub fn new(protocol_id: ProtocolId, priority: Priority, raw_msg: Vec<u8>) -> Self {
        Self {
            protocol_id,
            priority,
            raw_msg,
        }
    }
}

impl IncomingRequest for DirectSendMsg {
    fn protocol_id(&self) -> crate::ProtocolId {
        self.protocol_id
    }

    fn data(&self) -> &Vec<u8> {
        &self.raw_msg
    }
}

/// Errors from reading and deserializing network messages off the wire.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("network message stream: failed to deserialize network message frame: {0}, frame length: {1}, frame prefix: {2:?}")]
    DeserializeError(#[source] bcs::Error, usize, Bytes),

    #[error("network message stream: IO error while reading message: {0}")]
    IoError(#[from] io::Error),
}

/// Errors from serializing and sending network messages on the wire.
#[derive(Debug, Error)]
pub enum WriteError {
    #[error("network message sink: failed to serialize network message: {0}")]
    SerializeError(#[source] bcs::Error),

    #[error("network message sink: IO error while sending message: {0}")]
    IoError(#[from] io::Error),
}

/// Returns a fully configured length-delimited codec for writing/reading
/// serialized [`NetworkMessage`] frames to/from a socket.
pub fn network_message_frame_codec(max_frame_size: usize) -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(max_frame_size)
        .length_field_length(4)
        .big_endian()
        .new_codec()
}

/// A `Stream` of inbound `MultiplexMessage`s read and deserialized from an
/// underlying socket.
#[pin_project]
pub struct MultiplexMessageStream<TReadSocket: AsyncRead + Unpin> {
    #[pin]
    framed_read: FramedRead<Compat<TReadSocket>, LengthDelimitedCodec>,
}

impl<TReadSocket: AsyncRead + Unpin> MultiplexMessageStream<TReadSocket> {
    pub fn new(socket: TReadSocket, max_frame_size: usize) -> Self {
        let frame_codec = network_message_frame_codec(max_frame_size);
        let compat_socket = socket.compat();
        let framed_read = FramedRead::new(compat_socket, frame_codec);
        Self { framed_read }
    }
}

impl<TReadSocket: AsyncRead + Unpin> Stream for MultiplexMessageStream<TReadSocket> {
    type Item = Result<MultiplexMessage, ReadError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.project().framed_read.poll_next(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                let frame = frame.freeze();

                match bcs::from_bytes(&frame) {
                    Ok(message) => Poll::Ready(Some(Ok(message))),
                    // Failed to deserialize the NetworkMessage
                    Err(err) => {
                        let mut frame = frame;
                        let frame_len = frame.len();
                        // Keep a few bytes from the frame for debugging
                        frame.truncate(8);
                        let err = ReadError::DeserializeError(err, frame_len, frame);
                        Poll::Ready(Some(Err(err)))
                    },
                }
            },
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(ReadError::IoError(err)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A `Sink` of outbound `NetworkMessage`s that will be serialized and sent over
/// an underlying socket.
#[pin_project]
pub struct MultiplexMessageSink<TWriteSocket: AsyncWrite> {
    #[pin]
    framed_write: FramedWrite<Compat<TWriteSocket>, LengthDelimitedCodec>,
}

impl<TWriteSocket: AsyncWrite> MultiplexMessageSink<TWriteSocket> {
    pub fn new(socket: TWriteSocket, max_frame_size: usize) -> Self {
        let frame_codec = network_message_frame_codec(max_frame_size);
        let compat_socket = socket.compat_write();
        let framed_write = FramedWrite::new(compat_socket, frame_codec);
        Self { framed_write }
    }
}

#[cfg(test)]
impl<TWriteSocket: AsyncWrite + Unpin> MultiplexMessageSink<TWriteSocket> {
    pub async fn send_raw_frame(&mut self, frame: Bytes) -> Result<(), WriteError> {
        use futures::sink::SinkExt;
        self.framed_write
            .send(frame)
            .await
            .map_err(WriteError::IoError)
    }
}

impl<TWriteSocket: AsyncWrite> Sink<&MultiplexMessage> for MultiplexMessageSink<TWriteSocket> {
    type Error = WriteError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project()
            .framed_write
            .poll_ready(cx)
            .map_err(WriteError::IoError)
    }

    fn start_send(self: Pin<&mut Self>, message: &MultiplexMessage) -> Result<(), Self::Error> {
        let frame = bcs::to_bytes(message).map_err(WriteError::SerializeError)?;
        let frame = Bytes::from(frame);

        self.project()
            .framed_write
            .start_send(frame)
            .map_err(WriteError::IoError)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project()
            .framed_write
            .poll_flush(cx)
            .map_err(WriteError::IoError)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project()
            .framed_write
            .poll_close(cx)
            .map_err(WriteError::IoError)
    }
}
