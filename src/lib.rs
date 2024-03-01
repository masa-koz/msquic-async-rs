#![warn(unreachable_pub)]
#![warn(clippy::use_self)]

use once_cell::sync::Lazy;

static MSQUIC_API: Lazy<msquic::Api> = Lazy::new(|| msquic::Api::new());

mod connection;
mod stream;
mod listener;

pub use connection::{Connection, ConnectionError};
pub use stream::{Stream, StartError, ReadError, WriteError};
pub use listener::{Listener, ListenError};