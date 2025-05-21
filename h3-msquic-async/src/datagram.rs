//! Support for the h3-datagram crate.
//!
//! This module implements the traits defined in h3-datagram for the msquic-async crate.
use std::future::poll_fn;
use std::sync::Arc;
use std::task::{ready, Poll};

use futures::{stream, StreamExt};
use h3_datagram::datagram::EncodedDatagram;
use h3_datagram::quic_traits::{
    DatagramConnectionExt, RecvDatagram, SendDatagram, SendDatagramErrorIncoming,
};

use h3_datagram::ConnectionErrorIncoming;

use bytes::{Buf, Bytes};

use crate::{convert_connection_error, BoxStreamSync, Connection};

/// A Struct which allows to send datagrams over a QUIC connection.
pub struct SendDatagramHandler {
    conn: msquic_async::Connection,
}

impl<B: Buf> SendDatagram<B> for SendDatagramHandler {
    fn send_datagram<T: Into<EncodedDatagram<B>>>(
        &mut self,
        data: T,
    ) -> Result<(), SendDatagramErrorIncoming> {
        let mut buf: EncodedDatagram<B> = data.into();
        self.conn
            .send_datagram(&buf.copy_to_bytes(buf.remaining()))
            .map_err(convert_dgram_send_error)
    }
}

/// A Struct which allows to receive datagrams over a QUIC connection.
pub struct RecvDatagramHandler {
    datagrams: BoxStreamSync<'static, Result<Bytes, msquic_async::DgramReceiveError>>,
}

impl RecvDatagram for RecvDatagramHandler {
    type Buffer = Bytes;
    fn poll_incoming_datagram(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::Buffer, ConnectionErrorIncoming>> {
        Poll::Ready(
            ready!(self.datagrams.poll_next_unpin(cx))
                .expect("self. datagrams never returns None")
                .map_err(convert_dgram_recv_error),
        )
    }
}

impl<B: Buf> DatagramConnectionExt<B> for Connection {
    type SendDatagramHandler = SendDatagramHandler;
    type RecvDatagramHandler = RecvDatagramHandler;

    fn send_datagram_handler(&self) -> Self::SendDatagramHandler {
        SendDatagramHandler {
            conn: self.conn.clone(),
        }
    }

    fn recv_datagram_handler(&self) -> Self::RecvDatagramHandler {
        RecvDatagramHandler {
            datagrams: Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((poll_fn(|cx| conn.poll_receive_datagram(cx)).await, conn))
            })),
        }
    }
}

fn convert_dgram_send_error(error: msquic_async::DgramSendError) -> SendDatagramErrorIncoming {
    match error {
        msquic_async::DgramSendError::Denied => SendDatagramErrorIncoming::NotAvailable,
        msquic_async::DgramSendError::TooBig => SendDatagramErrorIncoming::TooLarge,
        msquic_async::DgramSendError::ConnectionLost(e) => {
            SendDatagramErrorIncoming::ConnectionError(convert_h3_error_to_datagram_error(
                convert_connection_error(e),
            ))
        }
        error @ msquic_async::DgramSendError::ConnectionNotStarted
        | error @ msquic_async::DgramSendError::OtherError(_) => {
            SendDatagramErrorIncoming::ConnectionError(convert_h3_error_to_datagram_error(
                h3::quic::ConnectionErrorIncoming::Undefined(Arc::new(error)),
            ))
        }
    }
}

fn convert_dgram_recv_error(error: msquic_async::DgramReceiveError) -> ConnectionErrorIncoming {
    match error {
        msquic_async::DgramReceiveError::ConnectionLost(e) => {
            convert_h3_error_to_datagram_error(convert_connection_error(e))
        }
        error @ msquic_async::DgramReceiveError::ConnectionNotStarted
        | error @ msquic_async::DgramReceiveError::OtherError(_) => {
            convert_h3_error_to_datagram_error(h3::quic::ConnectionErrorIncoming::Undefined(
                Arc::new(error),
            ))
        }
    }
}

fn convert_h3_error_to_datagram_error(
    error: h3::quic::ConnectionErrorIncoming,
) -> ConnectionErrorIncoming {
    match error {
        h3::quic::ConnectionErrorIncoming::ApplicationClose { error_code } => {
            ConnectionErrorIncoming::ApplicationClose { error_code }
        }
        h3::quic::ConnectionErrorIncoming::Timeout => h3_datagram::ConnectionErrorIncoming::Timeout,
        h3::quic::ConnectionErrorIncoming::InternalError(err) => {
            ConnectionErrorIncoming::InternalError(err)
        }
        h3::quic::ConnectionErrorIncoming::Undefined(error) => {
            ConnectionErrorIncoming::Undefined(error)
        }
    }
}
