//! `omtvideosink`: publishes video as an OMT source.

use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;
use gst_video::prelude::*;

mod imp {
    use std::sync::Mutex;

    use gst::subclass::prelude::*;
    use gst_base::subclass::prelude::*;
    use gst_video::VideoFormat;

    use super::*;
    use crate::shared::{self, CAT, Quality, SharedSender};
    use omt::PixelFormat;

    #[derive(Clone)]
    pub(crate) struct Settings {
        pub omt_name: String,
        pub port: u32,
        pub quality: Quality,
        pub advertise: bool,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                omt_name: "GStreamer".into(),
                port: 0,
                quality: Quality::Standard,
                advertise: true,
            }
        }
    }

    struct State {
        sender: SharedSender,
        info: Option<gst_video::VideoInfo>,
        scratch: Vec<u8>,
    }

    #[derive(Default)]
    pub struct OmtVideoSink {
        settings: Mutex<Settings>,
        state: Mutex<Option<State>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OmtVideoSink {
        const NAME: &'static str = "GstOmtVideoSink";
        type Type = super::OmtVideoSink;
        type ParentType = gst_base::BaseSink;
    }

    pub(crate) fn sink_properties() -> Vec<glib::ParamSpec> {
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
        ]
    }

    pub(crate) fn set_sink_property(
        settings: &mut Settings,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "omt-name" => {
                settings.omt_name = value.get::<Option<String>>().unwrap().unwrap_or_default()
            }
            "port" => settings.port = value.get().unwrap(),
            "quality" => settings.quality = value.get().unwrap(),
            "advertise" => settings.advertise = value.get().unwrap(),
            _ => unreachable!(),
        }
    }

    pub(crate) fn sink_property(settings: &Settings, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "omt-name" => settings.omt_name.to_value(),
            "port" => settings.port.to_value(),
            "quality" => settings.quality.to_value(),
            "advertise" => settings.advertise.to_value(),
            _ => unreachable!(),
        }
    }

    impl ObjectImpl for OmtVideoSink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: std::sync::LazyLock<Vec<glib::ParamSpec>> =
                std::sync::LazyLock::new(sink_properties);
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            set_sink_property(&mut self.settings.lock().unwrap(), value, pspec);
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            sink_property(&self.settings.lock().unwrap(), pspec)
        }

        fn constructed(&self) {
            self.parent_constructed();
            // Render on time: OMT receivers schedule by timestamp, and a
            // non-live upstream (a file) must not be sent faster than real time.
            self.obj().set_sync(true);
        }
    }

    impl GstObjectImpl for OmtVideoSink {}

    impl ElementImpl for OmtVideoSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: std::sync::LazyLock<gst::subclass::ElementMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "OMT Video Sink",
                        "Sink/Network/Video",
                        "Publishes video as an Open Media Transport source",
                        "Meros <https://meros.co>",
                    )
                });
            Some(&META)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::LazyLock<Vec<gst::PadTemplate>> =
                std::sync::LazyLock::new(|| {
                    let caps = gst_video::VideoCapsBuilder::new()
                        .format_list([VideoFormat::Uyvy, VideoFormat::Bgra, VideoFormat::Bgrx])
                        .build();
                    vec![
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &caps,
                        )
                        .unwrap(),
                    ]
                });
            TEMPLATES.as_ref()
        }
    }

    impl BaseSinkImpl for OmtVideoSink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let s = self.settings.lock().unwrap().clone();
            let sender = shared::sender(&s.omt_name, s.port as u16, s.quality, s.advertise)
                .map_err(|e| {
                    gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["could not start OMT sender {}: {}", s.omt_name, e]
                    )
                })?;
            gst::info!(
                CAT,
                imp = self,
                "publishing {} on port {}",
                s.omt_name,
                sender.lock().unwrap().port()
            );
            *self.state.lock().unwrap() = Some(State {
                sender,
                info: None,
                scratch: Vec::new(),
            });
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            *self.state.lock().unwrap() = None;
            Ok(())
        }

        fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
            let info = gst_video::VideoInfo::from_caps(caps)
                .map_err(|_| gst::loggable_error!(CAT, "invalid caps {caps}"))?;
            let mut state = self.state.lock().unwrap();
            let state = state
                .as_mut()
                .ok_or_else(|| gst::loggable_error!(CAT, "not started"))?;
            let fps = info.fps();
            state.sender.lock().unwrap().set_video_format(
                info.width() as i32,
                info.height() as i32,
                (fps.numer().max(1), fps.denom().max(1)),
            );
            state.info = Some(info);
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let mut guard = self.state.lock().unwrap();
            let state = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            let info = state.info.as_ref().ok_or(gst::FlowError::NotNegotiated)?;
            let format = match info.format() {
                VideoFormat::Uyvy => PixelFormat::Uyvy,
                _ => PixelFormat::Bgra,
            };
            let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer.as_ref(), info)
                .map_err(|_| gst::FlowError::Error)?;
            let stride = frame.plane_stride()[0];
            let data = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
            // VMX's API takes a mutable buffer; the frame is read-only.
            state.scratch.clear();
            state.scratch.extend_from_slice(data);
            let ts = running_ticks(&*self.obj(), buffer);
            let outcome = state
                .sender
                .lock()
                .unwrap()
                .send_video(format, &mut state.scratch, stride, ts)
                .map_err(|e| {
                    gst::element_imp_error!(self, gst::StreamError::Encode, ["{}", e]);
                    gst::FlowError::Error
                })?;
            if outcome.dropped > 0 {
                gst::debug!(
                    CAT,
                    imp = self,
                    "{} receiver(s) behind; frame dropped for them",
                    outcome.dropped
                );
            }
            Ok(gst::FlowSuccess::Ok)
        }
    }

    /// A buffer's running time in OMT ticks (100 ns), shared by video and
    /// audio so receivers can sync them.
    pub(crate) fn running_ticks(
        sink: &impl IsA<gst_base::BaseSink>,
        buffer: &gst::BufferRef,
    ) -> i64 {
        let segment = sink.as_ref().segment();
        let rt = buffer.pts().and_then(|pts| {
            segment
                .downcast_ref::<gst::ClockTime>()
                .and_then(|s| s.to_running_time(pts))
        });
        rt.map(|t| (t.nseconds() / 100) as i64).unwrap_or(-1).max(0)
    }
}

glib::wrapper! {
    pub struct OmtVideoSink(ObjectSubclass<imp::OmtVideoSink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub(crate) use imp::{
    Settings as SinkSettings, running_ticks, set_sink_property, sink_properties, sink_property,
};

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtvideosink",
        gst::Rank::NONE,
        OmtVideoSink::static_type(),
    )
}
