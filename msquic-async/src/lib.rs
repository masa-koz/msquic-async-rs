#![warn(unreachable_pub)]
#![warn(clippy::use_self)]

use once_cell::sync::Lazy;

static MSQUIC_API: Lazy<msquic::Api> = Lazy::new(|| msquic::Api::new());

mod buffer;
mod connection;
mod credential;
mod listener;
mod stream;

pub use connection::{
    Connection, ConnectionError, ShutdownError as ConnectionShutdownError,
    StartError as ConnectionStartError,
};
pub use credential::{CredentialConfig, CredentialConfigCertFile};
pub use listener::{ListenError, Listener};
pub use stream::{
    ReadError, ReadStream, StartError as StreamStartError, Stream, StreamType, WriteError,
    WriteStream,
};
pub use buffer::StreamRecvBuffer;

#[cfg(test)]
mod tests;
