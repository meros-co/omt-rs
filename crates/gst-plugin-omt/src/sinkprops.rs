//! Properties common to `omtsink`, `omtvideosink` and `omtaudiosink`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use gst::glib;
use gst::prelude::*;

use crate::shared::{self, DEFAULT_STATS_INTERVAL_MS, Quality, SinkShared};

#[derive(Clone)]
pub struct SinkSettings {
    pub omt_name: String,
    pub port: u32,
    pub quality: Quality,
    pub advertise: bool,
    pub info: omt::SenderInfo,
    pub stats_interval: u32,
}

impl Default for SinkSettings {
    fn default() -> Self {
        Self {
            omt_name: "GStreamer".into(),
            port: 0,
            quality: Quality::Standard,
            advertise: true,
            info: omt::SenderInfo::default(),
            stats_interval: DEFAULT_STATS_INTERVAL_MS,
        }
    }
}

impl SinkSettings {
    /// The sender for these settings, created if needed, with this element's
    /// sender info and statistics interval applied.
    pub fn start(&self, element: &gst::Element) -> Result<Arc<SinkShared>, gst::ErrorMessage> {
        let shared = shared::sender(
            &self.omt_name,
            self.port as u16,
            self.quality,
            self.advertise,
        )
        .map_err(|e| {
            gst::error_msg!(
                gst::ResourceError::OpenWrite,
                ["could not start OMT sender {}: {}", self.omt_name, e]
            )
        })?;
        shared.claim_owner(element);
        self.apply(&shared);
        Ok(shared)
    }

    fn apply(&self, shared: &SinkShared) {
        if self.info != omt::SenderInfo::default() {
            shared
                .sender
                .lock()
                .unwrap()
                .set_sender_info(Some(&self.info));
        }
        shared
            .stats_interval
            .store(self.stats_interval, Ordering::Relaxed);
    }

    fn running(&self) -> Option<Arc<SinkShared>> {
        shared::existing_sender(&self.omt_name, self.port as u16)
    }
}

pub fn properties() -> Vec<glib::ParamSpec> {
    vec![
        glib::ParamSpecString::builder("omt-name")
            .nick("OMT name")
            .blurb("Source name to publish; receivers see \"MACHINE (omt-name)\"")
            .default_value(Some("GStreamer"))
            .mutable_ready()
            .build(),
        glib::ParamSpecUInt::builder("port")
            .nick("Port")
            .blurb("TCP port to listen on (0 = 6960, or any free port if taken)")
            .maximum(65535)
            .mutable_ready()
            .build(),
        glib::ParamSpecEnum::builder_with_default("quality", Quality::Standard)
            .nick("Quality")
            .blurb("VMX quality tier (bitrate); never changes the resolution")
            .mutable_ready()
            .build(),
        glib::ParamSpecBoolean::builder("advertise")
            .nick("Advertise")
            .blurb("Publish the source over mDNS so receivers can find it by name")
            .default_value(true)
            .mutable_ready()
            .build(),
        glib::ParamSpecString::builder("product-name")
            .nick("Product name")
            .blurb("Sender info: the product name receivers see")
            .mutable_playing()
            .build(),
        glib::ParamSpecString::builder("manufacturer")
            .nick("Manufacturer")
            .blurb("Sender info: the manufacturer receivers see")
            .mutable_playing()
            .build(),
        glib::ParamSpecString::builder("version")
            .nick("Version")
            .blurb("Sender info: the version receivers see")
            .mutable_playing()
            .build(),
        glib::ParamSpecUInt::builder("stats-interval")
            .nick("Statistics interval")
            .blurb("Post an omt-stats bus message this often, in ms (0 = never)")
            .default_value(DEFAULT_STATS_INTERVAL_MS)
            .mutable_playing()
            .build(),
        glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
            .nick("Statistics")
            .blurb("Connections, frames, drops, bitrate and tally (see omt-stats)")
            .read_only()
            .build(),
        glib::ParamSpecBoolean::builder("tally-program")
            .nick("Program tally")
            .blurb("True while any receiver has this source on program")
            .read_only()
            .build(),
        glib::ParamSpecBoolean::builder("tally-preview")
            .nick("Preview tally")
            .blurb("True while any receiver has this source on preview")
            .read_only()
            .build(),
    ]
}

pub fn set(settings: &mut SinkSettings, value: &glib::Value, pspec: &glib::ParamSpec) {
    let text = || value.get::<Option<String>>().unwrap().unwrap_or_default();
    match pspec.name() {
        "omt-name" => settings.omt_name = text(),
        "port" => settings.port = value.get().unwrap(),
        "quality" => settings.quality = value.get().unwrap(),
        "advertise" => settings.advertise = value.get().unwrap(),
        "product-name" => settings.info.product_name = text(),
        "manufacturer" => settings.info.manufacturer = text(),
        "version" => settings.info.version = text(),
        "stats-interval" => settings.stats_interval = value.get().unwrap(),
        other => unreachable!("{other}"),
    }
    // Sender info and the interval may change while running.
    if matches!(
        pspec.name(),
        "product-name" | "manufacturer" | "version" | "stats-interval"
    ) {
        if let Some(shared) = settings.running() {
            settings.apply(&shared);
        }
    }
}

pub fn get(settings: &SinkSettings, pspec: &glib::ParamSpec) -> glib::Value {
    let tally = || {
        settings
            .running()
            .map(|s| s.sender.lock().unwrap().tally())
            .unwrap_or_default()
    };
    match pspec.name() {
        "omt-name" => settings.omt_name.to_value(),
        "port" => settings.port.to_value(),
        "quality" => settings.quality.to_value(),
        "advertise" => settings.advertise.to_value(),
        "product-name" => settings.info.product_name.to_value(),
        "manufacturer" => settings.info.manufacturer.to_value(),
        "version" => settings.info.version.to_value(),
        "stats-interval" => settings.stats_interval.to_value(),
        "stats" => settings
            .running()
            .map(|s| s.stats())
            .unwrap_or_else(|| gst::Structure::new_empty("omt-stats"))
            .to_value(),
        "tally-program" => tally().program.to_value(),
        "tally-preview" => tally().preview.to_value(),
        other => unreachable!("{other}"),
    }
}

/// A buffer's running time in OMT ticks (100 ns), shared by video and audio
/// so receivers can sync them.
pub fn running_ticks(sink: &impl IsA<gst_base::BaseSink>, buffer: &gst::BufferRef) -> i64 {
    use gst_base::prelude::*;
    let segment = sink.as_ref().segment();
    let rt = buffer.pts().and_then(|pts| {
        segment
            .downcast_ref::<gst::ClockTime>()
            .and_then(|s| s.to_running_time(pts))
    });
    rt.map(|t| (t.nseconds() / 100) as i64).unwrap_or(-1).max(0)
}
