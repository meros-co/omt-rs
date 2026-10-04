//! `omtaudiosink`: publishes audio as an OMT source (alongside an
//! `omtvideosink` of the same `omt-name`, they form one source).

use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;

mod imp {
    use std::sync::{Arc, Mutex};

    use gst::subclass::prelude::*;
    use gst_base::subclass::prelude::*;

    use super::*;
    use crate::shared::{CAT, SinkShared};
    use crate::sinkprops::{self, SinkSettings, running_ticks};

    struct State {
        shared: Arc<SinkShared>,
        info: Option<gst_audio::AudioInfo>,
        planar: Vec<f32>,
    }

    #[derive(Default)]
    pub struct OmtAudioSink {
        settings: Mutex<SinkSettings>,
        state: Mutex<Option<State>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OmtAudioSink {
        const NAME: &'static str = "GstOmtAudioSink";
        type Type = super::OmtAudioSink;
        type ParentType = gst_base::BaseSink;
    }

    impl ObjectImpl for OmtAudioSink {
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
            self.obj().set_sync(true);
        }
    }

    impl GstObjectImpl for OmtAudioSink {}

    impl ElementImpl for OmtAudioSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: std::sync::LazyLock<gst::subclass::ElementMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "OMT Audio Sink",
                        "Sink/Network/Audio",
                        "Publishes audio as an Open Media Transport source",
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

    impl BaseSinkImpl for OmtAudioSink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let settings = self.settings.lock().unwrap().clone();
            let shared = settings.start(self.obj().upcast_ref())?;
            gst::info!(
                CAT,
                imp = self,
                "publishing audio for {}",
                settings.omt_name
            );
            *self.state.lock().unwrap() = Some(State {
                shared,
                info: None,
                planar: Vec::new(),
            });
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            *self.state.lock().unwrap() = None;
            Ok(())
        }

        fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
            let info = gst_audio::AudioInfo::from_caps(caps)
                .map_err(|_| gst::loggable_error!(CAT, "invalid caps {caps}"))?;
            let mut state = self.state.lock().unwrap();
            state
                .as_mut()
                .ok_or_else(|| gst::loggable_error!(CAT, "not started"))?
                .info = Some(info);
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let mut guard = self.state.lock().unwrap();
            let state = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            let info = state.info.as_ref().ok_or(gst::FlowError::NotNegotiated)?;
            let channels = info.channels() as usize;
            let rate = info.rate() as i32;
            state.planar.clear();
            if info.layout() == gst_audio::AudioLayout::NonInterleaved {
                // Already OMT's layout, but the planes may sit anywhere in the
                // buffer (GstAudioMeta), so read them one by one.
                let abuf =
                    gst_audio::AudioBufferRef::from_buffer_ref_readable(buffer.as_ref(), info)
                        .map_err(|_| gst::FlowError::Error)?;
                for c in 0..channels as u32 {
                    let plane = abuf.plane_data(c).map_err(|_| gst::FlowError::Error)?;
                    state.planar.extend(
                        plane
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                    );
                }
            } else {
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let samples: Vec<f32> = map
                    .as_slice()
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                let spc = samples.len() / channels.max(1);
                for c in 0..channels {
                    state
                        .planar
                        .extend((0..spc).map(|i| samples[i * channels + c]));
                }
            }
            let ts = running_ticks(&*self.obj(), buffer);
            state
                .shared
                .sender
                .lock()
                .unwrap()
                .send_audio(rate, channels as i32, &state.planar, ts)
                .map_err(|e| {
                    gst::element_imp_error!(self, gst::ResourceError::Write, ["{}", e]);
                    gst::FlowError::Error
                })?;
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

glib::wrapper! {
    pub struct OmtAudioSink(ObjectSubclass<imp::OmtAudioSink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub(crate) fn caps() -> gst::Caps {
    let mut caps = gst_audio::AudioCapsBuilder::new_interleaved()
        .format(gst_audio::AudioFormat::F32le)
        .channels_range(1..=32)
        .build();
    caps.merge(
        gst_audio::AudioCapsBuilder::new()
            .format(gst_audio::AudioFormat::F32le)
            .layout(gst_audio::AudioLayout::NonInterleaved)
            .channels_range(1..=32)
            .build(),
    );
    caps
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtaudiosink",
        gst::Rank::NONE,
        OmtAudioSink::static_type(),
    )
}
