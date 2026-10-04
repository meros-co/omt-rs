//! `omtaudiosrc`: receives an OMT source's audio as interleaved F32.

use gst::glib;
use gst::prelude::*;
use gst_base::prelude::*;

mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use gst::subclass::prelude::*;
    use gst_base::subclass::base_src::CreateSuccess;
    use gst_base::subclass::prelude::*;

    use super::*;
    use crate::shared::{CAT, TimeBase};
    use omt::receive::{BlockingReceiver, Frame, ReceiverOptions};

    struct Running {
        receiver: BlockingReceiver,
        caps_for: Option<(i32, i32)>,
    }

    #[derive(Default)]
    pub struct OmtAudioSrc {
        source: Mutex<String>,
        running: Mutex<Option<Running>>,
        flushing: AtomicBool,
        time_base: Mutex<Option<Arc<TimeBase>>>,
    }

    impl OmtAudioSrc {
        pub fn set_time_base(&self, time_base: Arc<TimeBase>) {
            *self.time_base.lock().unwrap() = Some(time_base);
        }

        fn time_base(&self) -> Arc<TimeBase> {
            self.time_base
                .lock()
                .unwrap()
                .get_or_insert_with(TimeBase::new)
                .clone()
        }

        fn running_time_now(&self) -> gst::ClockTime {
            let obj = self.obj();
            match (obj.clock(), obj.base_time()) {
                (Some(clock), Some(base)) => clock.time().saturating_sub(base),
                _ => gst::ClockTime::ZERO,
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OmtAudioSrc {
        const NAME: &'static str = "GstOmtAudioSrc";
        type Type = super::OmtAudioSrc;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for OmtAudioSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: std::sync::LazyLock<Vec<glib::ParamSpec>> =
                std::sync::LazyLock::new(|| {
                    vec![
                        glib::ParamSpecString::builder("source")
                            .nick("Source")
                            .blurb(
                                "OMT source: a discovered name (\"MACHINE (Name)\") or host:port",
                            )
                            .mutable_ready()
                            .build(),
                    ]
                });
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "source" => {
                    *self.source.lock().unwrap() =
                        value.get::<Option<String>>().unwrap().unwrap_or_default()
                }
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "source" => self.source.lock().unwrap().to_value(),
                _ => unreachable!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_live(true);
            obj.set_format(gst::Format::Time);
        }
    }

    impl GstObjectImpl for OmtAudioSrc {}

    impl ElementImpl for OmtAudioSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: std::sync::LazyLock<gst::subclass::ElementMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "OMT Audio Source",
                        "Source/Network/Audio",
                        "Receives audio from an Open Media Transport source",
                        "Meros <https://meros.co>",
                    )
                });
            Some(&META)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::LazyLock<Vec<gst::PadTemplate>> =
                std::sync::LazyLock::new(|| {
                    let caps = gst_audio::AudioCapsBuilder::new_interleaved()
                        .format(gst_audio::AudioFormat::F32le)
                        .channels_range(1..=32)
                        .build();
                    vec![
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &caps,
                        )
                        .unwrap(),
                    ]
                });
            TEMPLATES.as_ref()
        }
    }

    impl BaseSrcImpl for OmtAudioSrc {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let source = self.source.lock().unwrap().clone();
            if source.is_empty() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["no source set"]
                ));
            }
            let options = ReceiverOptions {
                video: false,
                audio: true,
                quality: None,
            };
            let receiver = BlockingReceiver::connect(&source, options).map_err(|e| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["OMT source {}: {}", source, e]
                )
            })?;
            receiver
                .set_read_timeout(Some(Duration::from_millis(100)))
                .map_err(|e| gst::error_msg!(gst::ResourceError::OpenRead, ["{}", e]))?;
            gst::info!(
                CAT,
                imp = self,
                "connected to {} ({})",
                source,
                receiver.address()
            );
            self.time_base().reset();
            *self.running.lock().unwrap() = Some(Running {
                receiver,
                caps_for: None,
            });
            self.flushing.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            if let Some(r) = self.running.lock().unwrap().take() {
                r.receiver.shutdown();
            }
            Ok(())
        }

        fn is_seekable(&self) -> bool {
            false
        }

        /// Caps come from the stream (set on the first frame and on every
        /// format change). The default would fixate the template up front -
        /// a 1x1 picture or 1 Hz audio - and force a renegotiation.
        fn negotiate(&self) -> Result<(), gst::LoggableError> {
            Ok(())
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            self.flushing.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            self.flushing.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    impl PushSrcImpl for OmtAudioSrc {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
            let mut guard = self.running.lock().unwrap();
            let running = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            let audio = loop {
                if self.flushing.load(Ordering::SeqCst) {
                    return Err(gst::FlowError::Flushing);
                }
                match running.receiver.next_frame() {
                    Ok(Frame::Audio(a)) if a.header.samples_per_channel > 0 => break a,
                    Ok(_) => continue,
                    Err(omt::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Read,
                            ["OMT receive failed: {}", e]
                        );
                        return Err(gst::FlowError::Error);
                    }
                }
            };
            let (rate, channels) = (audio.header.sample_rate, audio.header.channels.clamp(1, 32));
            let spc = audio.header.samples_per_channel as usize;
            if running.caps_for != Some((rate, channels)) {
                let info = gst_audio::AudioInfo::builder(
                    gst_audio::AudioFormat::F32le,
                    rate as u32,
                    channels as u32,
                )
                .build()
                .map_err(|_| gst::FlowError::NotNegotiated)?;
                let caps = info.to_caps().map_err(|_| gst::FlowError::NotNegotiated)?;
                drop(guard);
                self.obj()
                    .set_caps(&caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                guard = self.running.lock().unwrap();
                guard.as_mut().ok_or(gst::FlowError::Flushing)?.caps_for = Some((rate, channels));
            }
            drop(guard);

            // OMT audio is planar; GStreamer's default layout is interleaved.
            let ch = channels as usize;
            let mut buffer =
                gst::Buffer::with_size(spc * ch * 4).map_err(|_| gst::FlowError::Error)?;
            {
                let buf = buffer.get_mut().unwrap();
                {
                    let mut map = buf.map_writable().map_err(|_| gst::FlowError::Error)?;
                    let out = map.as_mut_slice();
                    for i in 0..spc {
                        for c in 0..ch {
                            let s = audio.planar.get(c * spc + i).copied().unwrap_or(0.0);
                            out[(i * ch + c) * 4..][..4].copy_from_slice(&s.to_le_bytes());
                        }
                    }
                }
                let pts = self
                    .time_base()
                    .running_time(audio.timestamp, self.running_time_now());
                buf.set_pts(pts);
                buf.set_duration(
                    gst::ClockTime::SECOND.mul_div_floor(spc as u64, rate.max(1) as u64),
                );
            }
            Ok(CreateSuccess::NewBuffer(buffer))
        }
    }
}

glib::wrapper! {
    pub struct OmtAudioSrc(ObjectSubclass<imp::OmtAudioSrc>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl OmtAudioSrc {
    pub(crate) fn set_time_base(&self, time_base: std::sync::Arc<crate::shared::TimeBase>) {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        self.imp().set_time_base(time_base);
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtaudiosrc",
        gst::Rank::NONE,
        OmtAudioSrc::static_type(),
    )
}
