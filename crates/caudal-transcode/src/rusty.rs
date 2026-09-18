//! The `RustyH264` engine (placeholder until implemented).

use std::sync::Arc;

use caudal_core::{Registry, Subscriber};

use crate::{Source, TranscodeConfig};

pub(crate) async fn run(_registry: &Arc<Registry>, _sub: Subscriber, src: Source, _cfg: &TranscodeConfig) {
    tracing::error!(stream = %src.name, "rusty_h264 engine not implemented");
}
