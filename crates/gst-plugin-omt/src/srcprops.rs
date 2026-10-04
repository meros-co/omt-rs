//! Properties common to `omtsrc`, `omtvideosrc` and `omtaudiosrc`.

use std::sync::atomic::Ordering;

use gst::glib;
use gst::prelude::*;

use crate::shared::{DEFAULT_STATS_INTERVAL_MS, Quality, SourceShared};

/// Default reported latency: room for network jitter and VMX decode.
pub const DEFAULT_LATENCY_MS: u32 = 50;

#[derive(Clone)]
pub struct SrcSettings {
    pub source: String,
    pub quality: Quality,
    pub alpha: bool,
    pub latency_ms: u32,
}

impl Default for SrcSettings {
    fn default() -> Self {
        Self {
            source: String::new(),
            quality: Quality::Standard,
            alpha: false,
            latency_ms: DEFAULT_LATENCY_MS,
        }
    }
}

/// `video` adds the video-only properties (`quality`, `alpha`).
pub fn properties(video: bool) -> Vec<glib::ParamSpec> {
    let mut props = vec![
        glib::ParamSpecString::builder("source")
            .nick("Source")
            .blurb("OMT source: a discovered name (\"MACHINE (Name)\") or host:port")
            .mutable_ready()
            .build(),
        glib::ParamSpecBoolean::builder("tally-program")
            .nick("Program tally")
            .blurb("Tell the source it is on program here")
            .mutable_playing()
            .build(),
        glib::ParamSpecBoolean::builder("tally-preview")
            .nick("Preview tally")
            .blurb("Tell the source it is on preview here")
            .mutable_playing()
            .build(),
        glib::ParamSpecBoolean::builder("drift-correction")
            .nick("Drift correction")
            .blurb(
                "Lock timestamps to the pipeline clock, correcting drift between the sender's \
                 clock and ours (audio is resampled by the measured ppm)",
            )
            .default_value(true)
            .mutable_ready()
            .build(),
        glib::ParamSpecUInt::builder("latency")
            .nick("Latency")
            .blurb("Latency to report, in ms: room for network jitter and decoding")
            .default_value(super::srcprops::DEFAULT_LATENCY_MS)
            .maximum(10_000)
            .mutable_ready()
            .build(),
        glib::ParamSpecUInt::builder("stats-interval")
            .nick("Statistics interval")
            .blurb("Post an omt-stats bus message this often, in ms (0 = never)")
            .default_value(DEFAULT_STATS_INTERVAL_MS)
            .mutable_playing()
            .build(),
        glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
            .nick("Statistics")
            .blurb("Frames, drops, bitrate, drift, jitter and tally (see omt-stats)")
            .read_only()
            .build(),
        glib::ParamSpecString::builder("sender-product-name")
            .nick("Sender product")
            .blurb("Product name the source reports, if any")
            .read_only()
            .build(),
        glib::ParamSpecString::builder("sender-manufacturer")
            .nick("Sender manufacturer")
            .blurb("Manufacturer the source reports, if any")
            .read_only()
            .build(),
        glib::ParamSpecString::builder("sender-version")
            .nick("Sender version")
            .blurb("Version the source reports, if any")
            .read_only()
            .build(),
    ];
    if video {
        props.push(
            glib::ParamSpecEnum::builder_with_default("quality", Quality::Standard)
                .nick("Quality")
                .blurb("Quality to ask the sender for (standard = sender's choice)")
                .mutable_ready()
                .build(),
        );
        props.push(
            glib::ParamSpecBoolean::builder("alpha")
                .nick("Alpha")
                .blurb("Output BGRA, keeping the sender's alpha channel, instead of UYVY")
                .mutable_ready()
                .build(),
        );
    }
    props
}

pub fn set(
    settings: &mut SrcSettings,
    shared: &SourceShared,
    value: &glib::Value,
    pspec: &glib::ParamSpec,
) {
    match pspec.name() {
        "source" => settings.source = value.get::<Option<String>>().unwrap().unwrap_or_default(),
        "quality" => settings.quality = value.get().unwrap(),
        "alpha" => settings.alpha = value.get().unwrap(),
        "latency" => settings.latency_ms = value.get().unwrap(),
        "tally-program" => {
            let mut t = shared.tally();
            t.program = value.get().unwrap();
            shared.set_tally(t);
        }
        "tally-preview" => {
            let mut t = shared.tally();
            t.preview = value.get().unwrap();
            shared.set_tally(t);
        }
        "drift-correction" => shared
            .drift_correction
            .store(value.get().unwrap(), Ordering::Relaxed),
        "stats-interval" => shared
            .stats_interval
            .store(value.get().unwrap(), Ordering::Relaxed),
        other => unreachable!("{other}"),
    }
}

pub fn get(settings: &SrcSettings, shared: &SourceShared, pspec: &glib::ParamSpec) -> glib::Value {
    let info = || shared.sender_info().unwrap_or_default();
    match pspec.name() {
        "source" => settings.source.to_value(),
        "quality" => settings.quality.to_value(),
        "alpha" => settings.alpha.to_value(),
        "latency" => settings.latency_ms.to_value(),
        "tally-program" => shared.tally().program.to_value(),
        "tally-preview" => shared.tally().preview.to_value(),
        "drift-correction" => shared.drift_correction.load(Ordering::Relaxed).to_value(),
        "stats-interval" => shared.stats_interval.load(Ordering::Relaxed).to_value(),
        "stats" => shared.stats().to_value(),
        "sender-product-name" => info().product_name.to_value(),
        "sender-manufacturer" => info().manufacturer.to_value(),
        "sender-version" => info().version.to_value(),
        other => unreachable!("{other}"),
    }
}

/// Answers a latency query for a live OMT source.
pub fn latency_query(query: &mut gst::QueryRef, latency_ms: u32) -> bool {
    if let gst::QueryViewMut::Latency(q) = query.view_mut() {
        q.set(
            true,
            gst::ClockTime::from_mseconds(latency_ms as u64),
            gst::ClockTime::NONE,
        );
        return true;
    }
    false
}
