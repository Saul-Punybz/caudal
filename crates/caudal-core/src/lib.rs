//! Core of the Caudal media server: the media model and the live buffer
//! that sits between one publisher and many viewers.

pub mod gate;
pub mod media;
pub mod net;
pub mod registry;
pub mod stream;

pub use gate::{Access, Denied, Gate, GateFuture};
pub use media::{AudioParams, Codec, Cue, CueKind, Frame, TrackId, TrackInfo, TrackKind, VideoParams};
pub use net::Cidr;
pub use registry::{DEMAND_TIMEOUT, Demand, DemandFuture, PublishError, Publisher, Registry};
pub use stream::{BufferConfig, Event, PushError, StartAt, Stream, StreamStats, Subscriber};
