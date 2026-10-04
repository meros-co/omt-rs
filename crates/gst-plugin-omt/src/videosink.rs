//! `omtvideosink`: publishes video as an OMT source.

use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;

mod imp {
    use std::sync::{Arc, Mutex};

    use gst::subclass::prelude::*;
    use gst_base::subclass::prelude::*;
    use gst_video::VideoFormat;
    use gst_video::prelude::*;

    use super::*;
    use crate::shared::{CAT, SinkShared};
    use crate::sinkprops::{self, SinkSettings, running_ticks};
    use omt::PixelFormat;

    struct State {
        shared: Arc<SinkShared>,
        info: Option<gst_video::VideoInfo>,
        scratch: Vec<u8>,
    }

    #[derive(Default)]
    pub struct OmtVideoSink {
        settings: Mutex<SinkSettings>,
        state: Mutex<Option<State>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OmtVideoSink {
        const NAME: &'static str = "GstOmtVideoSink";
        type Type = super::OmtVideoSink;
        type ParentType = gst_base::BaseSink;
    }

    impl ObjectImpl for OmtVideoSink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: std::sync::LazyLock<Vec<glib::ParamSpec>> =
                std::sync::LazyLock::new(sinkprops::properties);
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            sinkprops::set(&mut self.settings.lock().unwrap(), value, pspec);
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            sinkprops::get(&self.settings.lock().unwrap(), pspec)
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
                    vec![
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &super::caps(),
                        )
                        .unwrap(),
                    ]
                });
            TEMPLATES.as_ref()
        }
    }

    impl BaseSinkImpl for OmtVideoSink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let settings = self.settings.lock().unwrap().clone();
            let shared = settings.start(self.obj().upcast_ref())?;
            gst::info!(
                CAT,
                imp = self,
                "publishing {} on port {}",
                settings.omt_name,
                shared.sender.lock().unwrap().port()
            );
            *self.state.lock().unwrap() = Some(State {
                shared,
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
            state.shared.sender.lock().unwrap().set_video_format(
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
                .shared
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
}

glib::wrapper! {
    pub struct OmtVideoSink(ObjectSubclass<imp::OmtVideoSink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub(crate) fn caps() -> gst::Caps {
    gst_video::VideoCapsBuilder::new()
        .format_list([
            gst_video::VideoFormat::Uyvy,
            gst_video::VideoFormat::Bgra,
            gst_video::VideoFormat::Bgrx,
        ])
        .build()
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtvideosink",
        gst::Rank::NONE,
        OmtVideoSink::static_type(),
    )
}
