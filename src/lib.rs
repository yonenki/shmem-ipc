mod channel;
mod error;
mod header;
mod platform;
mod ring;
mod wait;

pub use channel::{Channel, ChannelConfig, Role};
pub use header::ChannelState;
pub use error::{Error, Result};
pub use ring::DEFAULT_RING_DATA_SIZE;
pub use wait::{SpinOnly, SpinThenWait, WaitStrategy};
