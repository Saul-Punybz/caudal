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
/// `stream` and `output` (the OMT source name, unique per output);
/// `caudal_omt_frames_dropped_total` carries `direction` (`pull` or
/// `output`) and `reason` on top.
pub fn render_omt(out: &mut String, pulls: &[caudal_omt::PullStatus], outputs: &[caudal_omt::OutputStatus]) {
    use std::sync::atomic::Ordering::Relaxed;
    if pulls.is_empty() && outputs.is_empty() {
        return;
    }
    let pull_series: [(&str, &str, &str, fn(&caudal_omt::PullStats) -> u64); 4] = [
        ("caudal_omt_frames_in_total", "counter", "Video frames received from an OMT source.", |s| {
            s.frames_in.load(Relaxed)
        }),
        ("caudal_omt_bytes_in_total", "counter", "Bytes received from an OMT source (video and audio).", |s| {
            s.bytes_in.load(Relaxed)
        }),
        ("caudal_omt_reconnects_total", "counter", "Reconnections to an OMT source after the first connect.", |s| {
            s.reconnects.load(Relaxed)
        }),
        ("caudal_omt_connected", "gauge", "1 while connected to the OMT source.", |s| {
            u64::from(s.connected.load(Relaxed))
        }),
    ];
    for (name, kind, help, value) in pull_series {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
        for p in pulls {
            let _ = writeln!(out, "{name}{{stream=\"{}\"}} {}", escape(&p.stream), value(&p.stats));
        }
    }

    let _ = writeln!(out, "# HELP caudal_omt_frames_out_total Video frames sent as an OMT source.");
    let _ = writeln!(out, "# TYPE caudal_omt_frames_out_total counter");
    for o in outputs {
        let _ = writeln!(
            out,
            "caudal_omt_frames_out_total{{stream=\"{}\",output=\"{}\"}} {}",
            escape(&o.stream),
            escape(&o.name),
            o.stats.frames_sent.load(Relaxed)
        );
    }
    let _ = writeln!(out, "# HELP caudal_omt_receivers OMT receivers of an output's video now.");
    let _ = writeln!(out, "# TYPE caudal_omt_receivers gauge");
    for o in outputs {
        let _ = writeln!(
            out,
            "caudal_omt_receivers{{stream=\"{}\",output=\"{}\"}} {}",
            escape(&o.stream),
            escape(&o.name),
            o.stats.receivers.load(Relaxed)
        );
    }
    let _ = writeln!(out, "# HELP caudal_omt_tally 1 while any receiver has the output on this tally state.");
    let _ = writeln!(out, "# TYPE caudal_omt_tally gauge");
    for o in outputs {
        for (state, on) in [("preview", &o.stats.preview), ("program", &o.stats.program)] {
            let _ = writeln!(
                out,
                "caudal_omt_tally{{stream=\"{}\",output=\"{}\",state=\"{state}\"}} {}",
                escape(&o.stream),
                escape(&o.name),
                u8::from(on.load(Relaxed))
            );
        }
    }

    let _ = writeln!(out, "# HELP caudal_omt_frames_dropped_total OMT frames dropped, by direction and reason.");
    let _ = writeln!(out, "# TYPE caudal_omt_frames_dropped_total counter");
    for p in pulls {
        for (reason, n) in p.stats.dropped() {
            let _ = writeln!(
                out,
                "caudal_omt_frames_dropped_total{{direction=\"pull\",stream=\"{}\",reason=\"{reason}\"}} {n}",
                escape(&p.stream)
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

    #[test]
    fn renders_omt_series() {
        use std::sync::atomic::Ordering::Relaxed;
        let mut out = String::new();
        render_omt(&mut out, &[], &[]);
        assert!(out.is_empty(), "nothing without pulls or outputs: {out}");

        let registry = Registry::new();
        let pulls = caudal_omt::start_pulls(
            registry.clone(),
            caudal_core::BufferConfig::default(),
            vec![caudal_omt::PullConfig {
                stream: "cam1".into(),
                source: "STUDIO (Camera 1)".into(),
                quality: open_media_transport::command::Quality::High,
                video_kbps: 6000,
                audio_kbps: 128,
                ffmpeg: "ffmpeg".into(),
                discovery: None,
            }],
        );
        let outputs = caudal_omt::start_outputs(
            registry,
            vec![caudal_omt::OutputConfig {
                stream: "show".into(),
                name: "Program \"A\"".into(),
                quality: open_media_transport::command::Quality::Default,
                encoder_threads: 0,
                discovery: None,
            }],
        );
        let (p, o) = (pulls.status(), outputs.status());
        p[0].stats.frames_in.store(600, Relaxed);
        p[0].stats.reconnects.store(2, Relaxed);
        p[0].stats.dropped_queue_full.store(3, Relaxed);
        p[0].stats.connected.store(true, Relaxed);
        o[0].stats.receivers.store(2, Relaxed);
        o[0].stats.program.store(true, Relaxed);
        o[0].stats.dropped_decode.store(1, Relaxed);
        render_omt(&mut out, &p, &o);
        for line in [
            "caudal_omt_frames_in_total{stream=\"cam1\"} 600",
            "caudal_omt_reconnects_total{stream=\"cam1\"} 2",
            "caudal_omt_connected{stream=\"cam1\"} 1",
            "caudal_omt_frames_dropped_total{direction=\"pull\",stream=\"cam1\",reason=\"queue_full\"} 3",
            "caudal_omt_receivers{stream=\"show\",output=\"Program \\\"A\\\"\"} 2",
            "caudal_omt_tally{stream=\"show\",output=\"Program \\\"A\\\"\",state=\"program\"} 1",
            "caudal_omt_tally{stream=\"show\",output=\"Program \\\"A\\\"\",state=\"preview\"} 0",
            "caudal_omt_frames_dropped_total{direction=\"output\",stream=\"show\",output=\"Program \\\"A\\\"\",reason=\"decode_error\"} 1",
        ] {
            assert!(out.lines().any(|l| l == line), "missing {line:?} in:\n{out}");
        }
        // One HELP/TYPE per metric family.
        assert_eq!(out.matches("# TYPE caudal_omt_frames_dropped_total").count(), 1);
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
