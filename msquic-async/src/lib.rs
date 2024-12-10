#![warn(unreachable_pub)]
#![warn(clippy::use_self)]

mod buffer;
mod connection;
mod listener;
mod stream;

pub use connection::{
    Connection, ConnectionError, ShutdownError as ConnectionShutdownError,
    StartError as ConnectionStartError, DgramReceiveError, DgramSendError,
    OpenOutboundStream, AcceptInboundStream, AcceptInboundUniStream,
};
pub use listener::{ListenError, Listener};
pub use stream::{
    ReadError, ReadStream, StartError as StreamStartError, Stream, StreamType, WriteError,
    WriteStream,
};
pub use buffer::StreamRecvBuffer;

#[cfg(test)]
mod tests;
