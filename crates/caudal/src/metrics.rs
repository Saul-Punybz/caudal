//! Prometheus text exposition, rendered by hand from `Registry::list()` and
//! `Stream::stats()` on each scrape. No exporter dependency, no background
//! task: the numbers are always the live truth at request time.

use std::fmt::Write as _;

use caudal_core::Registry;

/// Renders the full `/metrics` body for the current state of `registry`.
/// `health` is `None` when `[health]` has no webhooks configured (the
/// watcher never starts, so there is nothing to report).
pub fn render(registry: &Registry, health: Option<&caudal_health::HealthService>) -> String {
    let streams = registry.list();
    let mut out = String::new();

    let _ = writeln!(out, "# HELP caudal_streams Number of currently live streams.");
    let _ = writeln!(out, "# TYPE caudal_streams gauge");
    let _ = writeln!(out, "caudal_streams {}", streams.len());

    let _ = writeln!(out, "# HELP caudal_viewers Current viewers of a stream.");
    let _ = writeln!(out, "# TYPE caudal_viewers gauge");
    for s in &streams {
        let stats = s.stats();
        let _ = writeln!(out, "caudal_viewers{{stream=\"{}\"}} {}", escape(s.name()), stats.viewers);
    }

    let _ = writeln!(out, "# HELP caudal_bytes_in_total Bytes ingested from the publisher.");
    let _ = writeln!(out, "# TYPE caudal_bytes_in_total counter");
    for s in &streams {
        let stats = s.stats();
        let _ = writeln!(out, "caudal_bytes_in_total{{stream=\"{}\"}} {}", escape(s.name()), stats.bytes_in);
    }

    let _ = writeln!(out, "# HELP caudal_frames_in_total Frames ingested from the publisher.");
    let _ = writeln!(out, "# TYPE caudal_frames_in_total counter");
    for s in &streams {
        let stats = s.stats();
        let _ = writeln!(out, "caudal_frames_in_total{{stream=\"{}\"}} {}", escape(s.name()), stats.frames_in);
    }

    if let Some(health) = health {
        let _ = writeln!(out, "# HELP caudal_alerts_active Stream health alerts currently active.");
        let _ = writeln!(out, "# TYPE caudal_alerts_active gauge");
        for m in health.metrics() {
            let _ = writeln!(out, "caudal_alerts_active{{rule=\"{}\"}} {}", m.rule, m.active);
        }
        let _ = writeln!(out, "# HELP caudal_alerts_fired_total Stream health alerts fired since start.");
        let _ = writeln!(out, "# TYPE caudal_alerts_fired_total counter");
        for m in health.metrics() {
            let _ = writeln!(out, "caudal_alerts_fired_total{{rule=\"{}\"}} {}", m.rule, m.fired_total);
        }
    }

    out
}

/// Escapes a label value per the Prometheus text format.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_zero_streams() {
        let registry = Registry::new();
        let body = render(&registry, None);
        assert!(body.contains("caudal_streams 0"), "{body}");
        assert!(body.contains("caudal_viewers"), "{body}");
        assert!(body.contains("caudal_bytes_in_total"), "{body}");
        assert!(body.contains("caudal_frames_in_total"), "{body}");
    }

    #[test]
    fn renders_a_live_stream() {
        let registry = Registry::new();
        let _publisher = registry.publish("test", caudal_core::BufferConfig::default()).unwrap();
        let body = render(&registry, None);
        assert!(body.contains("caudal_streams 1"), "{body}");
        assert!(body.contains("caudal_viewers{stream=\"test\"} 0"), "{body}");
        assert!(body.contains("caudal_bytes_in_total{stream=\"test\"} 0"), "{body}");
        assert!(body.contains("caudal_frames_in_total{stream=\"test\"} 0"), "{body}");
    }

    #[test]
    fn escapes_label_values() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[tokio::test]
    async fn renders_alert_metrics_when_health_is_running() {
        let registry = Registry::new();
        let health = caudal_health::start(
            registry.clone(),
            caudal_health::HealthConfig {
                no_keyframe_secs: Some(10),
                min_bitrate_kbps: None,
                min_bitrate_for_secs: 10,
                no_audio_secs: None,
                publisher_lost: true,
                publisher_lost_grace_secs: 5,
                min_hold_secs: 5,
                webhooks: vec!["http://127.0.0.1:0/hook".into()],
                secret: "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw".into(),
                overrides: Vec::new(),
            },
        )
        .unwrap();
        let body = render(&registry, Some(&health));
        assert!(body.contains("caudal_alerts_active{rule=\"no_keyframe\"} 0"), "{body}");
        assert!(body.contains("caudal_alerts_fired_total{rule=\"publisher_lost\"} 0"), "{body}");
    }
}
