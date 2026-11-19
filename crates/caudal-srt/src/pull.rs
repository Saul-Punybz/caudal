//! Pulls each configured [`crate::SrtPull`]: Caudal calls a remote SRT
//! listener (`srt://host:port?streamid=...`), and publishes the MPEG-TS it
//! receives as `stream`, the same way an accepted `publish/...` caller is.
//! A dropped connection ends the stream; the next connection publishes it
//! again. Reconnects with exponential backoff, reset once data flowed.

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, PublishError, Registry};
use caudal_ts::ingest::TsIngest;

use crate::SrtPull;

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Runs one pull until its task is aborted (a reload removing it, or
/// shutdown).
pub(crate) async fn run(pull: SrtPull, registry: Arc<Registry>, buffer: BufferConfig) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match crate::push::connect(&pull.url).await {
            Ok(mut socket) => match registry.publish(&pull.stream, buffer) {
                Ok(publisher) => {
                    tracing::info!(url = %pull.url, stream = %pull.stream, "srt pull connected");
                    let mut ingest = TsIngest::new(publisher);
                    let mut got_any = false;
                    loop {
                        match socket.recv().await {
                            Ok(Some(payload)) => {
                                got_any = true;
                                ingest.feed(&payload);
                            }
                            Ok(None) | Err(_) => break,
                        }
                    }
                    drop(ingest);
                    tracing::info!(url = %pull.url, stream = %pull.stream, "srt pull ended; reconnecting");
                    backoff = if got_any { BACKOFF_MIN } else { (backoff * 2).min(BACKOFF_MAX) };
                }
                Err(PublishError::Busy(_)) => {
                    tracing::warn!(stream = %pull.stream, "srt pull: stream already has a publisher; retrying");
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
                Err(PublishError::InvalidName) => {
                    tracing::error!(stream = %pull.stream, "srt pull: invalid stream name; giving up");
                    return;
                }
            },
            Err(err) => {
                tracing::warn!(url = %pull.url, stream = %pull.stream, %err, "srt pull connect failed");
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}
