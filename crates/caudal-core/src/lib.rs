//! Core of the Caudal media server: the media model and the live buffer
//! that sits between one publisher and many viewers.

pub mod captions;
pub mod gate;
pub mod media;
pub mod net;
pub mod registry;
pub mod session;
pub mod stream;

pub use gate::{Access, Denied, Gate, GateFuture, Rewrite, RewriteFuture, RewriteRequest, StreamKeys};
pub use media::{AudioParams, Codec, Cue, CueKind, Frame, TrackId, TrackInfo, TrackKind, VideoParams};
pub use net::Cidr;
pub use registry::{DEMAND_TIMEOUT, Demand, DemandFuture, PublishError, Publisher, Registry};
pub use session::{KickResult, ProtocolTotals, Quality, Selector, Session, SessionInfo, Sessions};
pub use stream::{
    BufferConfig, BufferTotals, Event, PushError, StartAt, Stream, StreamStats, Subscriber, buffer_totals,
};
