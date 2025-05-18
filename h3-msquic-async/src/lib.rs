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
use h3::quic::{self, Error, StreamId, WriteBuf};
#[cfg(feature = "datagram")]
use h3_datagram::{datagram::Datagram, quic_traits};
pub use msquic_async;
pub use msquic_async::msquic;
use std::fmt::{self, Display};
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

/// The error type for [`Connection`]
///
/// Wraps reasons a msquic connection might be lost.
#[derive(Debug)]
pub struct ConnectionError(msquic_async::ConnectionError);

impl std::error::Error for ConnectionError {}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for ConnectionError {
    fn is_timeout(&self) -> bool {
        if let msquic_async::ConnectionError::ShutdownByTransport(status, _) = &self.0 {
            matches!(
                status.try_as_status_code().unwrap(),
                msquic::StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT
                    | msquic::StatusCode::QUIC_STATUS_CONNECTION_IDLE
            )
        } else {
            false
        }
    }

    fn err_code(&self) -> Option<u64> {
        match self.0 {
            msquic_async::ConnectionError::ShutdownByPeer(error_code) => Some(error_code),
            _ => None,
        }
    }
}

impl From<msquic_async::ConnectionError> for ConnectionError {
    fn from(e: msquic_async::ConnectionError) -> Self {
        Self(e)
    }
}

#[derive(Debug)]
pub struct StreamStartError(msquic_async::StreamStartError);

impl std::error::Error for StreamStartError {}

impl fmt::Display for StreamStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for StreamStartError {
    fn is_timeout(&self) -> bool {
        if let msquic_async::StreamStartError::ConnectionLost(
            msquic_async::ConnectionError::ShutdownByTransport(status, _),
        ) = &self.0
        {
            matches!(
                status.try_as_status_code().unwrap(),
                msquic::StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT
                    | msquic::StatusCode::QUIC_STATUS_CONNECTION_IDLE
            )
        } else {
            false
        }
    }

    fn err_code(&self) -> Option<u64> {
        match self.0 {
            msquic_async::StreamStartError::ConnectionLost(
                msquic_async::ConnectionError::ShutdownByPeer(error_code),
            ) => Some(error_code),
            _ => None,
        }
    }
}

impl From<msquic_async::StreamStartError> for StreamStartError {
    fn from(e: msquic_async::StreamStartError) -> Self {
        Self(e)
    }
}

#[derive(Debug)]
pub struct RecvDatagramError(msquic_async::DgramReceiveError);

impl std::error::Error for RecvDatagramError {}

impl fmt::Display for RecvDatagramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for RecvDatagramError {
    fn is_timeout(&self) -> bool {
        if let msquic_async::DgramReceiveError::ConnectionLost(
            msquic_async::ConnectionError::ShutdownByTransport(status, _),
        ) = &self.0
        {
            matches!(
                status.try_as_status_code().unwrap(),
                msquic::StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT
                    | msquic::StatusCode::QUIC_STATUS_CONNECTION_IDLE
            )
        } else {
            false
        }
    }

    fn err_code(&self) -> Option<u64> {
        match self.0 {
            msquic_async::DgramReceiveError::ConnectionLost(
                msquic_async::ConnectionError::ShutdownByPeer(error_code),
            ) => Some(error_code),
            _ => None,
        }
    }
}

impl From<msquic_async::DgramReceiveError> for RecvDatagramError {
    fn from(e: msquic_async::DgramReceiveError) -> Self {
        Self(e)
    }
}

/// Types of errors when sending a datagram.
#[derive(Debug)]
pub enum SendDatagramError {
    /// The connection has not been started
    ConnectionNotStarted,
    /// Datagrams are not supported by the peer
    UnsupportedByPeer,
    /// Datagrams are locally disabled
    Disabled,
    /// The datagram was too large to be sent.
    TooLarge,
    /// Network error
    ConnectionLost(Box<dyn Error>),
    /// Other error
    OtherError(msquic::Status),
}

impl fmt::Display for SendDatagramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendDatagramError::ConnectionNotStarted => write!(f, "connection not started"),
            SendDatagramError::UnsupportedByPeer => write!(f, "datagrams not supported by peer"),
            SendDatagramError::Disabled => write!(f, "datagram support disabled"),
            SendDatagramError::TooLarge => write!(f, "datagram too large"),
            SendDatagramError::ConnectionLost(_) => write!(f, "connection lost"),
            SendDatagramError::OtherError(status) => write!(f, "other error: {}", status),
        }
    }
}

impl std::error::Error for SendDatagramError {}

impl Error for SendDatagramError {
    fn is_timeout(&self) -> bool {
        false
    }

    fn err_code(&self) -> Option<u64> {
        match self {
            Self::ConnectionLost(err) => err.err_code(),
            _ => None,
        }
    }
}

impl From<msquic_async::DgramSendError> for SendDatagramError {
    fn from(value: msquic_async::DgramSendError) -> Self {
        match value {
            msquic_async::DgramSendError::ConnectionNotStarted => Self::ConnectionNotStarted,
            msquic_async::DgramSendError::Denied => Self::UnsupportedByPeer,
            msquic_async::DgramSendError::TooBig => Self::TooLarge,
            msquic_async::DgramSendError::ConnectionLost(err) => {
                Self::ConnectionLost(ConnectionError::from(err).into())
            }
            msquic_async::DgramSendError::OtherError(status) => Self::OtherError(status),
        }
    }
}

impl<B> quic::Connection<B> for Connection
where
    B: Buf,
{
    type RecvStream = RecvStream;
    type OpenStreams = OpenStreams;
    type AcceptError = StreamStartError;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_accept_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::BidiStream>, Self::AcceptError>> {
        let stream = match ready!(self.incoming.poll_next_unpin(cx)) {
            Some(x) => x?,
            None => return Poll::Ready(Ok(None)),
        };
        if let (Some(read), Some(write)) = stream.split() {
            Poll::Ready(Ok(Some(Self::BidiStream {
                send: Self::SendStream::new(write),
                recv: RecvStream::new(read),
            })))
        } else {
            unreachable!("msquic-async should always return a bidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_accept_recv(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::RecvStream>, Self::AcceptError>> {
        let recv = match ready!(self.incoming_uni.poll_next_unpin(cx)) {
            Some(x) => x?,
            None => return Poll::Ready(Ok(None)),
        };
        Poll::Ready(Ok(Some(Self::RecvStream::new(recv))))
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
    type OpenError = StreamStartError;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, Self::OpenError>> {
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

        let stream = ready!(self.opening.as_mut().unwrap().poll_next_unpin(cx)).unwrap()?;
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
    ) -> Poll<Result<Self::SendStream, Self::OpenError>> {
        if self.opening_uni.is_none() {
            self.opening_uni = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((
                    conn.open_outbound_stream(msquic_async::StreamType::Unidirectional, false)
                        .await,
                    conn,
                ))
            })));
        }

        let stream = ready!(self.opening_uni.as_mut().unwrap().poll_next_unpin(cx)).unwrap()?;
        if let (None, Some(write)) = stream.split() {
            Poll::Ready(Ok(Self::SendStream::new(write)))
        } else {
            unreachable!("msquic-async should always return a unidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn close(&mut self, code: h3::error::Code, _reason: &[u8]) {
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
    type OpenError = StreamStartError;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, Self::OpenError>> {
        if self.opening.is_none() {
            self.opening = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((
                    conn.open_outbound_stream(msquic_async::StreamType::Bidirectional, false)
                        .await,
                    conn,
                ))
            })));
        }

        let stream = ready!(self.opening.as_mut().unwrap().poll_next_unpin(cx)).unwrap()?;
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
    ) -> Poll<Result<Self::SendStream, Self::OpenError>> {
        if self.opening_uni.is_none() {
            self.opening_uni = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((
                    conn.open_outbound_stream(msquic_async::StreamType::Unidirectional, false)
                        .await,
                    conn,
                ))
            })));
        }

        let stream = ready!(self.opening_uni.as_mut().unwrap().poll_next_unpin(cx)).unwrap()?;
        if let (None, Some(write)) = stream.split() {
            Poll::Ready(Ok(Self::SendStream::new(write)))
        } else {
            unreachable!("msquic-async should always return a unidirectional stream");
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn close(&mut self, code: h3::error::Code, _reason: &[u8]) {
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
    type Error = ReadError;

    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, Self::Error>> {
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
    type Error = SendStreamError;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.send.poll_ready(cx)
    }

    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.send.poll_finish(cx)
    }

    fn reset(&mut self, reset_code: u64) {
        self.send.reset(reset_code)
    }

    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), Self::Error> {
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
    ) -> Poll<Result<usize, Self::Error>> {
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
    type Error = ReadError;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, Self::Error>> {
        if let Some(stream) = self.stream.take() {
            self.read_chunk_fut.set(async move {
                let chunk = poll_fn(|cx| stream.poll_read_chunk(cx)).await;
                (stream, chunk)
            })
        };

        let (stream, chunk) = ready!(self.read_chunk_fut.poll(cx));
        self.stream = Some(stream);
        let chunk = chunk?.filter(|x| !x.is_empty() || !x.fin());
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

/// The error type for [`RecvStream`]
///
/// Wraps errors that occur when reading from a receive stream.
#[derive(Debug)]
pub struct ReadError(msquic_async::ReadError);

impl From<ReadError> for std::io::Error {
    fn from(value: ReadError) -> Self {
        value.0.into()
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<ReadError> for Arc<dyn Error> {
    fn from(e: ReadError) -> Self {
        Arc::new(e)
    }
}

impl From<msquic_async::ReadError> for ReadError {
    fn from(e: msquic_async::ReadError) -> Self {
        Self(e)
    }
}

impl Error for ReadError {
    fn is_timeout(&self) -> bool {
        if let msquic_async::ReadError::ConnectionLost(
            msquic_async::ConnectionError::ShutdownByTransport(status, _),
        ) = &self.0
        {
            matches!(
                status.try_as_status_code().unwrap(),
                msquic::StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT
                    | msquic::StatusCode::QUIC_STATUS_CONNECTION_IDLE
            )
        } else {
            false
        }
    }

    fn err_code(&self) -> Option<u64> {
        match self.0 {
            msquic_async::ReadError::ConnectionLost(
                msquic_async::ConnectionError::ShutdownByPeer(error_code),
            ) => Some(error_code),
            msquic_async::ReadError::Reset(error_code) => Some(error_code),
            _ => None,
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
    type Error = SendStreamError;

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
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
                        return Poll::Ready(Err(SendStreamError::Write(err)));
                    }
                }
            }
        }
        self.writing = None;
        Poll::Ready(Ok(()))
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.stream
            .as_mut()
            .unwrap()
            .poll_finish_write(cx)
            .map_err(|e| e.into())
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn reset(&mut self, reset_code: u64) {
        let _ = self.stream.as_mut().unwrap().abort_write(reset_code);
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all, level = "trace"))]
    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), Self::Error> {
        if self.writing.is_some() {
            return Err(Self::Error::NotReady);
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
    ) -> Poll<Result<usize, Self::Error>> {
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
            Err(err) => Poll::Ready(Err(SendStreamError::Write(err))),
        }
    }
}

/// The error type for [`SendStream`]
///
/// Wraps errors that can happen writing to or polling a send stream.
#[derive(Debug)]
pub enum SendStreamError {
    /// Errors when writing, wrapping a [`msquic_async::WriteError`]
    Write(msquic_async::WriteError),
    /// Error when the stream is not ready, because it is still sending
    /// data from a previous call
    NotReady,
    // /// Error when the stream is closed
    // StreamClosed(ClosedStream),
}

impl From<SendStreamError> for std::io::Error {
    fn from(value: SendStreamError) -> Self {
        match value {
            SendStreamError::Write(err) => err.into(),
            SendStreamError::NotReady => std::io::Error::other("send stream is not ready"),
        }
    }
}

impl std::error::Error for SendStreamError {}

impl Display for SendStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<msquic_async::WriteError> for SendStreamError {
    fn from(e: msquic_async::WriteError) -> Self {
        Self::Write(e)
    }
}

impl Error for SendStreamError {
    fn is_timeout(&self) -> bool {
        if let Self::Write(msquic_async::WriteError::ConnectionLost(
            msquic_async::ConnectionError::ShutdownByTransport(status, _),
        )) = &self
        {
            matches!(
                status.try_as_status_code().unwrap(),
                msquic::StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT
                    | msquic::StatusCode::QUIC_STATUS_CONNECTION_IDLE
            )
        } else {
            false
        }
    }

    fn err_code(&self) -> Option<u64> {
        match self {
            Self::Write(msquic_async::WriteError::Stopped(error_code)) => Some(*error_code),
            Self::Write(msquic_async::WriteError::ConnectionLost(
                msquic_async::ConnectionError::ShutdownByPeer(error_code),
            )) => Some(*error_code),
            _ => None,
        }
    }
}

impl From<SendStreamError> for Arc<dyn Error> {
    fn from(e: SendStreamError) -> Self {
        Arc::new(e)
    }
}
