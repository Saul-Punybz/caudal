//! Prometheus text exposition, rendered by hand from `Registry::list()` and
//! `Stream::stats()` on each scrape. No exporter dependency, no background
//! task: the numbers are always the live truth at request time.

use std::fmt::Write as _;

use caudal_core::Registry;

/// Renders the full `/metrics` body for the current state of `registry`.
pub fn render(registry: &Registry) -> String {
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
        let body = render(&registry);
        assert!(body.contains("caudal_streams 0"), "{body}");
        assert!(body.contains("caudal_viewers"), "{body}");
        assert!(body.contains("caudal_bytes_in_total"), "{body}");
        assert!(body.contains("caudal_frames_in_total"), "{body}");
    }

    #[test]
    fn renders_a_live_stream() {
        let registry = Registry::new();
        let _publisher = registry.publish("test", caudal_core::BufferConfig::default()).unwrap();
        let body = render(&registry);
        assert!(body.contains("caudal_streams 1"), "{body}");
        assert!(body.contains("caudal_viewers{stream=\"test\"} 0"), "{body}");
        assert!(body.contains("caudal_bytes_in_total{stream=\"test\"} 0"), "{body}");
        assert!(body.contains("caudal_frames_in_total{stream=\"test\"} 0"), "{body}");
    }

    #[test]
    fn escapes_label_values() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
