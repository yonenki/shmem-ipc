mod channel;
mod connection;
mod error;
mod header;
mod listener;
mod platform;
mod ring;
#[cfg(feature = "tokio")]
pub mod tokio;
mod wait;

pub use channel::{Channel, ChannelConfig, Role};
pub use connection::{RecvHalf, SendHalf, ShmemConnection};
pub use error::{Error, Result};
pub use header::ChannelState;
pub use listener::{ShmemListener, connect};
pub use ring::DEFAULT_RING_DATA_SIZE;
pub use wait::{SpinOnly, SpinThenWait, WaitStrategy};
