//! Core of the Caudal media server: the media model and the live buffer
//! that sits between one publisher and many viewers.

pub mod media;
pub mod registry;
pub mod stream;

pub use media::{AudioParams, Codec, Frame, TrackId, TrackInfo, TrackKind, VideoParams};
pub use registry::{PublishError, Publisher, Registry};
pub use stream::{BufferConfig, Event, PushError, StartAt, Stream, StreamStats, Subscriber};
