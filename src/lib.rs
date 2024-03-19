#![warn(unreachable_pub)]
#![warn(clippy::use_self)]

use once_cell::sync::Lazy;

static MSQUIC_API: Lazy<msquic::Api> = Lazy::new(|| msquic::Api::new());

mod buffer;
mod credential;
mod connection;
mod stream;
mod listener;


pub use credential::{CredentialConfig, CredentialConfigCertFile};
pub use connection::{Connection, ConnectionError, StartError as ConnectionStartError};
pub use stream::{Stream, ReadStream, WriteStream, StreamType, StartError as StreamStartError, ReadError, WriteError};
pub use listener::{Listener, ListenError};

#[cfg(test)]
mod tests;