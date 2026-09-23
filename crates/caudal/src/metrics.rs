//! Prometheus text exposition, rendered by hand from `Registry::list()` and
//! `Stream::stats()` on each scrape. No exporter dependency, no background
//! task: the numbers are always the live truth at request time.

use std::fmt::Write as _;

use caudal_core::Registry;

/// Renders the full `/metrics` body for the current state of `registry`.
/// `health` is `None` when `[health]` has no webhooks configured (the
/// watcher never starts, so there is nothing to report). `access` is
/// always `Some` in `crate::main::run` (a `caudal_access::Checker` is
/// created even with no `[[access.rules]]`); `None` only in a test that
/// builds `AppState` without calling `set_access`.
pub fn render(
    registry: &Registry,
    health: Option<&caudal_health::HealthService>,
    access: Option<&caudal_access::Checker>,
) -> String {
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

    if let Some(access) = access {
        let _ = writeln!(out, "# HELP caudal_access_denied_total Publish/play requests denied by [[access.rules]].");
        let _ = writeln!(out, "# TYPE caudal_access_denied_total counter");
        for m in access.metrics() {
            let _ = writeln!(
                out,
                "caudal_access_denied_total{{stream=\"{}\",reason=\"{}\"}} {}",
                escape(&m.stream),
                m.reason,
                m.count
            );
        }
    }

    out
}

/// Appends `caudal_omt_*` for every running `[[omt.pull]]` and
/// `[[omt.output]]`. Pull series are labelled by `stream`; output series by
/// `stream` and `output` (the OMT source name, unique per output).
/// `caudal_omt_frames_dropped_total` and `caudal_omt_tally`, which both
/// sides have, carry `direction` (`pull` or `output`) on top.
pub fn render_omt(out: &mut String, pulls: &[caudal_omt::PullStatus], outputs: &[caudal_omt::OutputStatus]) {
    use std::sync::atomic::Ordering::Relaxed;
    if pulls.is_empty() && outputs.is_empty() {
        return;
    }
    let pulls: Vec<(&str, caudal_omt::PullStatsSnapshot)> =
        pulls.iter().map(|p| (p.stream.as_str(), p.stats.snapshot())).collect();
    let header = |out: &mut String, name: &str, kind: &str, help: &str| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
    };

    header(out, "caudal_omt_frames_in_total", "counter", "Frames received from an OMT source, by kind.");
    for (stream, s) in &pulls {
        for (kind, n) in [("video", s.video_in), ("audio", s.audio_in)] {
            let _ = writeln!(out, "caudal_omt_frames_in_total{{stream=\"{}\",kind=\"{kind}\"}} {n}", escape(stream));
        }
    }
    type Series = (&'static str, &'static str, &'static str, fn(&caudal_omt::PullStatsSnapshot) -> u64);
    let pull_series: [Series; 4] = [
        ("caudal_omt_bytes_in_total", "counter", "Bytes received from an OMT source.", |s| s.bytes_in),
        ("caudal_omt_reconnects_total", "counter", "Reconnections to an OMT source after the first connect.", |s| {
            s.reconnects
        }),
        ("caudal_omt_publishes_total", "counter", "Times an OMT pull (re)published its stream.", |s| s.publishes),
        ("caudal_omt_connected", "gauge", "1 while every connection to the OMT source is up.", |s| {
            u64::from(s.connected)
        }),
    ];
    for (name, kind, help, value) in pull_series {
        header(out, name, kind, help);
        for (stream, s) in &pulls {
            let _ = writeln!(out, "{name}{{stream=\"{}\"}} {}", escape(stream), value(s));
        }
    }

    header(out, "caudal_omt_frames_out_total", "counter", "Video frames sent as an OMT source.");
    for o in outputs {
        let _ = writeln!(
            out,
            "caudal_omt_frames_out_total{{stream=\"{}\",output=\"{}\"}} {}",
            escape(&o.stream),
            escape(&o.name),
            o.stats.frames_sent.load(Relaxed)
        );
    }
    header(out, "caudal_omt_receivers", "gauge", "OMT receivers of an output's video now.");
    for o in outputs {
        let _ = writeln!(
            out,
            "caudal_omt_receivers{{stream=\"{}\",output=\"{}\"}} {}",
            escape(&o.stream),
            escape(&o.name),
            o.stats.receivers.load(Relaxed)
        );
    }

    header(
        out,
        "caudal_omt_tally",
        "gauge",
        "1 while on this tally state: for a pull, what Caudal tells the source; for an output, what any receiver tells Caudal.",
    );
    for (stream, s) in &pulls {
        for (state, on) in [("preview", s.tally.preview), ("program", s.tally.program)] {
            let _ = writeln!(
                out,
                "caudal_omt_tally{{direction=\"pull\",stream=\"{}\",state=\"{state}\"}} {}",
                escape(stream),
                u8::from(on)
            );
        }
    }
    for o in outputs {
        for (state, on) in [("preview", &o.stats.preview), ("program", &o.stats.program)] {
            let _ = writeln!(
                out,
                "caudal_omt_tally{{direction=\"output\",stream=\"{}\",output=\"{}\",state=\"{state}\"}} {}",
                escape(&o.stream),
                escape(&o.name),
                u8::from(on.load(Relaxed))
            );
        }
    }

    header(out, "caudal_omt_frames_dropped_total", "counter", "OMT frames dropped, by direction and reason.");
    for (stream, s) in &pulls {
        for (reason, n) in caudal_omt::DropReason::ALL.iter().zip(s.dropped) {
            let _ = writeln!(
                out,
                "caudal_omt_frames_dropped_total{{direction=\"pull\",stream=\"{}\",reason=\"{}\"}} {n}",
                escape(stream),
                reason.as_str()
            );
        }
    }
    for o in outputs {
        for (reason, n) in o.stats.dropped() {
            let _ = writeln!(
                out,
                "caudal_omt_frames_dropped_total{{direction=\"output\",stream=\"{}\",output=\"{}\",reason=\"{reason}\"}} {n}",
                escape(&o.stream),
                escape(&o.name)
            );
        }
    }
}

/// Escapes a label value per the Prometheus text format.
pub(crate) fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_zero_streams() {
        let registry = Registry::new();
        let body = render(&registry, None, None);
        assert!(body.contains("caudal_streams 0"), "{body}");
        assert!(body.contains("caudal_viewers"), "{body}");
        assert!(body.contains("caudal_bytes_in_total"), "{body}");
        assert!(body.contains("caudal_frames_in_total"), "{body}");
    }

    #[test]
    fn renders_a_live_stream() {
        let registry = Registry::new();
        let _publisher = registry.publish("test", caudal_core::BufferConfig::default()).unwrap();
        let body = render(&registry, None, None);
        assert!(body.contains("caudal_streams 1"), "{body}");
        assert!(body.contains("caudal_viewers{stream=\"test\"} 0"), "{body}");
        assert!(body.contains("caudal_bytes_in_total{stream=\"test\"} 0"), "{body}");
        assert!(body.contains("caudal_frames_in_total{stream=\"test\"} 0"), "{body}");
    }

    #[tokio::test]
    async fn renders_omt_series() {
        use std::sync::atomic::Ordering::Relaxed;
        let mut out = String::new();
        render_omt(&mut out, &[], &[]);
        assert!(out.is_empty(), "nothing without pulls or outputs: {out}");

        let registry = Registry::new();
        // A real pull pointed at a closed loopback port: it only ever fails
        // to connect, so the counters below are all ours.
        let pulls = caudal_omt::start_pulls(
            registry.clone(),
            caudal_core::BufferConfig::default(),
            vec![caudal_omt::PullConfig {
                stream: "cam1".into(),
                source: "omt://127.0.0.1:1".into(),
                quality: caudal_omt::Quality::High,
                video_kbps: 6000,
                audio_kbps: 128,
                ffmpeg: "ffmpeg".into(),
                directory: None,
            }],
        );
        let outputs = caudal_omt::start_outputs(
            registry,
            vec![caudal_omt::OutputConfig {
                stream: "show".into(),
                name: "Program \"A\"".into(),
                quality: caudal_omt::Quality::Default,
                encoder_threads: 0,
                discovery: None,
            }],
        );
        let (p, o) = (pulls.statuses(), outputs.status());
        p[0].stats.video_in.store(600, Relaxed);
        p[0].stats.audio_in.store(1000, Relaxed);
        p[0].stats.dropped[caudal_omt::DropReason::ALL.iter().position(|r| r.as_str() == "queue_full").unwrap()]
            .store(3, Relaxed);
        p[0].stats.tally.store(2, Relaxed);
        o[0].stats.receivers.store(2, Relaxed);
        o[0].stats.program.store(true, Relaxed);
        o[0].stats.dropped_decode.store(1, Relaxed);
        render_omt(&mut out, &p, &o);
        pulls.stop();
        for line in [
            "caudal_omt_frames_in_total{stream=\"cam1\",kind=\"video\"} 600",
            "caudal_omt_frames_in_total{stream=\"cam1\",kind=\"audio\"} 1000",
            "caudal_omt_frames_dropped_total{direction=\"pull\",stream=\"cam1\",reason=\"queue_full\"} 3",
            "caudal_omt_frames_dropped_total{direction=\"pull\",stream=\"cam1\",reason=\"decode\"} 0",
            "caudal_omt_tally{direction=\"pull\",stream=\"cam1\",state=\"program\"} 1",
            "caudal_omt_tally{direction=\"pull\",stream=\"cam1\",state=\"preview\"} 0",
            "caudal_omt_receivers{stream=\"show\",output=\"Program \\\"A\\\"\"} 2",
            "caudal_omt_tally{direction=\"output\",stream=\"show\",output=\"Program \\\"A\\\"\",state=\"program\"} 1",
            "caudal_omt_frames_dropped_total{direction=\"output\",stream=\"show\",output=\"Program \\\"A\\\"\",reason=\"decode_error\"} 1",
        ] {
            assert!(out.lines().any(|l| l == line), "missing {line:?} in:\n{out}");
        }
        for name in ["caudal_omt_reconnects_total", "caudal_omt_connected", "caudal_omt_bytes_in_total"] {
            assert!(out.contains(&format!("{name}{{stream=\"cam1\"}} ")), "{name}: {out}");
        }
        // One HELP/TYPE per metric family.
        assert_eq!(out.matches("# TYPE caudal_omt_frames_dropped_total").count(), 1);
        assert_eq!(out.matches("# TYPE caudal_omt_tally").count(), 1);
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
        let body = render(&registry, Some(&health), None);
        assert!(body.contains("caudal_alerts_active{rule=\"no_keyframe\"} 0"), "{body}");
        assert!(body.contains("caudal_alerts_fired_total{rule=\"publisher_lost\"} 0"), "{body}");
    }

    #[test]
    fn renders_access_denial_counters() {
        let registry = Registry::new();
        let rule = caudal_access::Rule {
            streams: vec!["*".into()],
            play_deny: vec![caudal_access::Entry::parse("1.2.3.4").unwrap()],
            ..Default::default()
        };
        let checker = caudal_access::AccessConfig { rules: vec![rule], geoip_db: None }.checker().unwrap();
        assert!(checker.check(caudal_core::Access::Play, "live", Some("1.2.3.4".parse().unwrap())).is_err());
        let body = render(&registry, None, Some(&checker));
        assert!(body.contains("caudal_access_denied_total{stream=\"live\",reason=\"ip_denied\"} 1"), "{body}");
    }
}
