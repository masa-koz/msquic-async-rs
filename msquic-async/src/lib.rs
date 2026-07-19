#![warn(unreachable_pub)]
#![warn(clippy::use_self)]

mod buffer;
mod connection;
mod listener;
mod stream;
mod sync;

#[cfg(feature = "msquic-latest")]
pub use msquic;
#[cfg(feature = "msquic-2-5")]
pub use msquic_v2_5 as msquic;
#[cfg(feature = "msquic-seera")]
pub use seera_msquic as msquic;

pub use buffer::StreamRecvBuffer;
pub use connection::{
    AcceptInboundStream, AcceptInboundUniStream, Connection, ConnectionError, ConnectionEvent,
    DgramReceiveError, DgramSendError, EventError, OpenOutboundStream,
    ShutdownError as ConnectionShutdownError, StartError as ConnectionStartError,
};
pub use listener::{ListenError, Listener};
pub use stream::{
    ReadError, ReadStream, StartError as StreamStartError, Stream, StreamType, WriteError,
    WriteStream,
};

#[cfg(test)]
mod tests;
