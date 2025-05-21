/// This file is based on the `lib.rs` from the `h3-quinn` crate.
use bytes::Buf;
#[cfg(feature = "datagram")]
use bytes::{Bytes, BytesMut};
use futures::{
    future::poll_fn,
    ready,
    stream::{self},
    Stream, StreamExt,
};
use h3::{
    error::Code,
    quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf},
};
#[cfg(feature = "datagram")]
use h3_datagram::{datagram::Datagram, quic_traits};
pub use msquic_async;
pub use msquic_async::msquic;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{self, Poll};
use tokio_util::sync::ReusableBoxFuture;
#[cfg(feature = "tracing")]
use tracing::instrument;

/// BoxStream with Sync trait
type BoxStreamSync<'a, T> = Pin<Box<dyn Stream<Item = T> + Sync + Send + 'a>>;

/// A QUIC connection backed by msquic-async
///
/// Implements a [`quic::Connection`] backed by a [`msquic_async::Connection`].
pub struct Connection {
    conn: msquic_async::Connection,
    incoming: BoxStreamSync<'static, Result<msquic_async::Stream, msquic_async::StreamStartError>>,
    opening: Option<
        BoxStreamSync<'static, Result<msquic_async::Stream, msquic_async::StreamStartError>>,
    >,
    incoming_uni:
        BoxStreamSync<'static, Result<msquic_async::ReadStream, msquic_async::StreamStartError>>,
    opening_uni: Option<
        BoxStreamSync<'static, Result<msquic_async::Stream, msquic_async::StreamStartError>>,
    >,
    #[cfg(feature = "datagram")]
    datagrams: BoxStreamSync<'static, Result<Bytes, msquic_async::DgramReceiveError>>,
}

impl Connection {
    /// Create a [`Connection`] from a [`msquic_async::Connection`]
    pub fn new(conn: msquic_async::Connection) -> Self {
        Self {
            conn: conn.clone(),
            incoming: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_inbound_stream().await, conn))
            })),
            opening: None,
            incoming_uni: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_inbound_uni_stream().await, conn))
            })),
            opening_uni: None,
            #[cfg(feature = "datagram")]
            datagrams: Box::pin(stream::unfold(conn, |conn| async {
                Some((poll_fn(|cx| conn.poll_receive_datagram(cx)).await, conn))
            })),
        }
    }
}

fn convert_connection_error(e: msquic_async::ConnectionError) -> ConnectionErrorIncoming {
    match e {
        msquic_async::ConnectionError::ShutdownByPeer(error_code) => {
            ConnectionErrorIncoming::ApplicationClose {
                error_code: error_code.into(),
            }
        }
        msquic_async::ConnectionError::ShutdownByTransport(status, code) => {
            if matches!(
                status.try_as_status_code().unwrap(),
                msquic::StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT
                    | msquic::StatusCode::QUIC_STATUS_CONNECTION_IDLE
            ) {
                ConnectionErrorIncoming::Timeout
            } else {
                ConnectionErrorIncoming::Undefined(Arc::new(
                    msquic_async::ConnectionError::ShutdownByTransport(status, code),
                ))
            }
        }

        error @ msquic_async::ConnectionError::ShutdownByLocal
        | error @ msquic_async::ConnectionError::ConnectionClosed
        | error @ msquic_async::ConnectionError::OtherError(_) => {
            ConnectionErrorIncoming::Undefined(Arc::new(error))
        }
    }
}

fn convert_start_error(e: msquic_async::StreamStartError) -> ConnectionErrorIncoming {
    match e {
        msquic_async::StreamStartError::ConnectionLost(error) => convert_connection_error(error),

        error @ msquic_async::StreamStartError::ConnectionNotStarted
        | error @ msquic_async::StreamStartError::LimitReached
        | error @ msquic_async::StreamStartError::OtherError(_) => {
            ConnectionErrorIncoming::Undefined(Arc::new(error))
        }
    }
}

impl<B> quic::Connection<B> for Connection
where
    B: Buf,
{
    type RecvStream = RecvStream;
    type OpenStreams = OpenStreams;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_accept_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        let stream = ready!(self.incoming.poll_next_unpin(cx))
            .expect("self.incoming BoxStream never returns None")
            .map_err(|e| convert_start_error(e))?;
        if let (Some(read), Some(write)) = stream.split() {
            Poll::Ready(Ok(Self::BidiStream {
                send: Self::SendStream::new(write),
                recv: RecvStream::new(read),
            }))
        } else {
            unreachable!("msquic-async should always return a bidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_accept_recv(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        let recv = ready!(self.incoming_uni.poll_next_unpin(cx))
            .expect("self.incoming_uni BoxStream never returns None")
            .map_err(|e| convert_start_error(e))?;
        Poll::Ready(Ok(Self::RecvStream::new(recv)))
    }

    fn opener(&self) -> Self::OpenStreams {
        OpenStreams {
            conn: self.conn.clone(),
            opening: None,
            opening_uni: None,
        }
    }
}

impl<B> quic::OpenStreams<B> for Connection
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        if self.opening.is_none() {
            self.opening = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((
                    conn.clone()
                        .open_outbound_stream(msquic_async::StreamType::Bidirectional, false)
                        .await,
                    conn,
                ))
            })));
        }

        let stream = ready!(self.opening.as_mut().unwrap().poll_next_unpin(cx))
            .unwrap()
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_start_error(e),
            })?;
        if let (Some(read), Some(write)) = stream.split() {
            Poll::Ready(Ok(Self::BidiStream {
                send: Self::SendStream::new(write),
                recv: RecvStream::new(read),
            }))
        } else {
            unreachable!("msquic-async should always return a bidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        if self.opening_uni.is_none() {
            self.opening_uni = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((
                    conn.open_outbound_stream(msquic_async::StreamType::Unidirectional, false)
                        .await,
                    conn,
                ))
            })));
        }

        let stream = ready!(self.opening_uni.as_mut().unwrap().poll_next_unpin(cx))
            .unwrap()
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_start_error(e),
            })?;
        if let (None, Some(write)) = stream.split() {
            Poll::Ready(Ok(Self::SendStream::new(write)))
        } else {
            unreachable!("msquic-async should always return a unidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn close(&mut self, code: Code, _reason: &[u8]) {
        self.conn.shutdown(code.value()).ok();
    }
}

#[cfg(feature = "datagram")]
impl<B> quic_traits::SendDatagramExt<B> for Connection
where
    B: Buf,
{
    type Error = SendDatagramError;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn send_datagram(&mut self, data: Datagram<B>) -> Result<(), SendDatagramError> {
        // TODO investigate static buffer from known max datagram size
        let mut buf = BytesMut::new();
        data.encode(&mut buf);
        self.conn.send_datagram(&buf.freeze())?;

        Ok(())
    }
}

#[cfg(feature = "datagram")]
impl quic_traits::RecvDatagramExt for Connection {
    type Buf = Bytes;

    type Error = RecvDatagramError;

    #[inline]
    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_accept_datagram(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, Self::Error>> {
        match ready!(self.datagrams.poll_next_unpin(cx)) {
            Some(Ok(x)) => Poll::Ready(Ok(Some(x))),
            Some(Err(e)) => Poll::Ready(Err(e.into())),
            None => Poll::Ready(Ok(None)),
        }
    }
}

/// Stream opener backed by a msquic connection
///
/// Implements [`quic::OpenStreams`] using [`msquic_async::Connection`],
/// [`msquic_async::OpenOutboundStream`].
pub struct OpenStreams {
    conn: msquic_async::Connection,
    opening: Option<
        BoxStreamSync<'static, Result<msquic_async::Stream, msquic_async::StreamStartError>>,
    >,
    opening_uni: Option<
        BoxStreamSync<'static, Result<msquic_async::Stream, msquic_async::StreamStartError>>,
    >,
}

impl<B> quic::OpenStreams<B> for OpenStreams
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        if self.opening.is_none() {
            self.opening = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((
                    conn.open_outbound_stream(msquic_async::StreamType::Bidirectional, false)
                        .await,
                    conn,
                ))
            })));
        }

        let stream = ready!(self.opening.as_mut().unwrap().poll_next_unpin(cx))
            .unwrap()
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_start_error(e),
            })?;
        if let (Some(read), Some(write)) = stream.split() {
            Poll::Ready(Ok(Self::BidiStream {
                send: Self::SendStream::new(write),
                recv: RecvStream::new(read),
            }))
        } else {
            unreachable!("msquic-async should always return a bidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        if self.opening_uni.is_none() {
            self.opening_uni = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((
                    conn.open_outbound_stream(msquic_async::StreamType::Unidirectional, false)
                        .await,
                    conn,
                ))
            })));
        }

        let stream = ready!(self.opening_uni.as_mut().unwrap().poll_next_unpin(cx))
            .unwrap()
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_start_error(e),
            })?;
        if let (None, Some(write)) = stream.split() {
            Poll::Ready(Ok(Self::SendStream::new(write)))
        } else {
            unreachable!("msquic-async should always return a unidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn close(&mut self, code: Code, _reason: &[u8]) {
        self.conn.shutdown(code.value()).ok();
    }
}

impl Clone for OpenStreams {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            opening: None,
            opening_uni: None,
        }
    }
}

/// msquic-backed bidirectional stream
///
/// Implements [`quic::BidiStream`] which allows the stream to be split
/// into two structs each implementing one direction.
pub struct BidiStream<B>
where
    B: Buf,
{
    send: SendStream<B>,
    recv: RecvStream,
}

impl<B> quic::BidiStream<B> for BidiStream<B>
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type RecvStream = RecvStream;

    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        (self.send, self.recv)
    }
}

impl<B: Buf> quic::RecvStream for BidiStream<B> {
    type Buf = msquic_async::StreamRecvBuffer;

    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        self.recv.poll_data(cx)
    }

    fn stop_sending(&mut self, error_code: u64) {
        self.recv.stop_sending(error_code)
    }

    fn recv_id(&self) -> StreamId {
        self.recv.recv_id()
    }
}

impl<B> quic::SendStream<B> for BidiStream<B>
where
    B: Buf,
{
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_ready(cx)
    }

    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_finish(cx)
    }

    fn reset(&mut self, reset_code: u64) {
        self.send.reset(reset_code)
    }

    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        self.send.send_data(data)
    }

    fn send_id(&self) -> StreamId {
        self.send.send_id()
    }
}
impl<B> quic::SendStreamUnframed<B> for BidiStream<B>
where
    B: Buf,
{
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        self.send.poll_send(cx, buf)
    }
}

/// msquic-backed receive stream
///
/// Implements a [`quic::RecvStream`] backed by a [`msquic_async::ReadStream`].
pub struct RecvStream {
    stream: Option<msquic_async::ReadStream>,
    read_chunk_fut: ReadChunkFuture,
}

type ReadChunkFuture = ReusableBoxFuture<
    'static,
    (
        msquic_async::ReadStream,
        Result<Option<msquic_async::StreamRecvBuffer>, msquic_async::ReadError>,
    ),
>;

impl RecvStream {
    fn new(stream: msquic_async::ReadStream) -> Self {
        Self {
            stream: Some(stream),
            // Should only allocate once the first time it's used
            read_chunk_fut: ReusableBoxFuture::new(async { unreachable!() }),
        }
    }
}

impl quic::RecvStream for RecvStream {
    type Buf = msquic_async::StreamRecvBuffer;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        if let Some(stream) = self.stream.take() {
            self.read_chunk_fut.set(async move {
                let chunk = poll_fn(|cx| stream.poll_read_chunk(cx)).await;
                (stream, chunk)
            })
        };

        let (stream, chunk) = ready!(self.read_chunk_fut.poll(cx));
        self.stream = Some(stream);
        let chunk = chunk
            .map_err(|e| convert_read_error_to_stream_error(e))?
            .filter(|x| !x.is_empty() || !x.fin());
        Poll::Ready(Ok(chunk))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn stop_sending(&mut self, error_code: u64) {
        self.stream.as_mut().unwrap().abort_read(error_code).ok();
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn recv_id(&self) -> StreamId {
        self.stream
            .as_ref()
            .unwrap()
            .id()
            .expect("id")
            .try_into()
            .expect("invalid stream id")
    }
}

fn convert_read_error_to_stream_error(error: msquic_async::ReadError) -> StreamErrorIncoming {
    match error {
        msquic_async::ReadError::Reset(error_code) => {
            StreamErrorIncoming::StreamTerminated { error_code }
        }
        msquic_async::ReadError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ msquic_async::ReadError::Closed
        | error @ msquic_async::ReadError::OtherError(_) => {
            StreamErrorIncoming::Unknown(Box::new(error))
        }
    }
}

/// msquic-async-backed send stream
///
/// Implements a [`quic::SendStream`] backed by a [`msquic_async::WriteStream`].
pub struct SendStream<B: Buf> {
    stream: Option<msquic_async::WriteStream>,
    writing: Option<WriteBuf<B>>,
    write_fut: WriteFuture,
}

type WriteFuture = ReusableBoxFuture<
    'static,
    (
        msquic_async::WriteStream,
        Result<usize, msquic_async::WriteError>,
    ),
>;

impl<B> SendStream<B>
where
    B: Buf,
{
    fn new(stream: msquic_async::WriteStream) -> SendStream<B> {
        Self {
            stream: Some(stream),
            writing: None,
            write_fut: ReusableBoxFuture::new(async { unreachable!() }),
        }
    }
}

impl<B> quic::SendStream<B> for SendStream<B>
where
    B: Buf,
{
    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        if let Some(ref mut data) = self.writing {
            while data.has_remaining() {
                if let Some(mut stream) = self.stream.take() {
                    let chunk = data.chunk().to_owned(); // FIXME - avoid copy
                    self.write_fut.set(async move {
                        let ret = poll_fn(|cx| stream.poll_write(cx, &chunk, false)).await;
                        (stream, ret)
                    });
                }

                let (stream, res) = ready!(self.write_fut.poll(cx));
                self.stream = Some(stream);
                match res {
                    Ok(cnt) => data.advance(cnt),
                    Err(err) => {
                        return Poll::Ready(Err(convert_write_error_to_stream_error(err)));
                    }
                }
            }
        }
        self.writing = None;
        Poll::Ready(Ok(()))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.stream
            .as_mut()
            .unwrap()
            .poll_finish_write(cx)
            .map_err(|e| convert_write_error_to_stream_error(e))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn reset(&mut self, reset_code: u64) {
        let _ = self.stream.as_mut().unwrap().abort_write(reset_code);
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        if self.writing.is_some() {
            // This can only happen if the traits are misused by h3 itself
            // If this happens log an error and close the connection with H3_INTERNAL_ERROR

            #[cfg(feature = "tracing")]
            tracing::error!("send_data called while send stream is not ready");
            return Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(
                    "internal error in the http stack".to_string(),
                ),
            });
        }
        self.writing = Some(data.into());
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn send_id(&self) -> StreamId {
        self.stream
            .as_ref()
            .unwrap()
            .id()
            .expect("id")
            .try_into()
            .expect("invalid stream id")
    }
}

impl<B> quic::SendStreamUnframed<B> for SendStream<B>
where
    B: Buf,
{
    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        if self.writing.is_some() {
            // This signifies a bug in implementation
            panic!("poll_send called while send stream is not ready")
        }

        let res = ready!(self
            .stream
            .as_mut()
            .unwrap()
            .poll_write(cx, buf.chunk(), false));
        match res {
            Ok(written) => {
                buf.advance(written);
                Poll::Ready(Ok(written))
            }
            Err(err) => Poll::Ready(Err(convert_write_error_to_stream_error(err))),
        }
    }
}

fn convert_write_error_to_stream_error(error: msquic_async::WriteError) -> StreamErrorIncoming {
    match error {
        msquic_async::WriteError::Stopped(error_code) => {
            StreamErrorIncoming::StreamTerminated { error_code }
        }
        msquic_async::WriteError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ msquic_async::WriteError::Closed
        | error @ msquic_async::WriteError::Finished
        | error @ msquic_async::WriteError::OtherError(_) => {
            StreamErrorIncoming::Unknown(Box::new(error))
        }
    }
}
