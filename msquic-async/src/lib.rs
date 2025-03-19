#![warn(unreachable_pub)]
#![warn(clippy::use_self)]

mod buffer;
mod connection;
mod listener;
mod stream;

pub use msquic;

pub use buffer::StreamRecvBuffer;
pub use connection::{
    AcceptInboundStream, AcceptInboundUniStream, Connection, ConnectionError, DgramReceiveError,
    DgramSendError, OpenOutboundStream, PathEvent, PathEventError,
    ShutdownError as ConnectionShutdownError, StartError as ConnectionStartError,
};
pub use listener::{ListenError, Listener};
pub use stream::{
    ReadError, ReadStream, StartError as StreamStartError, Stream, StreamType, WriteError,
    WriteStream,
};

#[cfg(test)]
mod tests;
