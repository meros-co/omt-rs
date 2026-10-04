//! `omtvideosrc`: receives an OMT source's video as raw frames.

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
    use gst_video::VideoFormat;

    use super::*;
    use crate::shared::{CAT, Quality, TimeBase};
    use omt::receive::{BlockingReceiver, Frame, ReceiverOptions};
    use omt::vmx::{DecodeFormat, VmxDecoder};

    #[derive(Clone)]
    struct Settings {
        source: String,
        quality: Quality,
        alpha: bool,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                source: String::new(),
                quality: Quality::Standard,
                alpha: false,
            }
        }
    }

    struct Running {
        receiver: BlockingReceiver,
        decoder: VmxDecoder,
        /// Width, height, framerate and format of the caps last set.
        caps_for: Option<(i32, i32, i32, i32, VideoFormat)>,
    }

    #[derive(Default)]
    pub struct OmtVideoSrc {
        settings: Mutex<Settings>,
        running: Mutex<Option<Running>>,
        flushing: AtomicBool,
        time_base: Mutex<Option<Arc<TimeBase>>>,
    }

    impl OmtVideoSrc {
        /// Shares a time base with a sibling audio source (set by `omtsrc`).
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
    impl ObjectSubclass for OmtVideoSrc {
        const NAME: &'static str = "GstOmtVideoSrc";
        type Type = super::OmtVideoSrc;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for OmtVideoSrc {
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
                        glib::ParamSpecEnum::builder_with_default("quality", Quality::Standard)
                            .nick("Quality")
                            .blurb("Quality to ask the sender for (standard = sender's choice)")
                            .mutable_ready()
                            .build(),
                        glib::ParamSpecBoolean::builder("alpha")
                            .nick("Alpha")
                            .blurb(
                                "Output BGRA, keeping the sender's alpha channel, instead of UYVY",
                            )
                            .mutable_ready()
                            .build(),
                    ]
                });
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let mut s = self.settings.lock().unwrap();
            match pspec.name() {
                "source" => s.source = value.get::<Option<String>>().unwrap().unwrap_or_default(),
                "quality" => s.quality = value.get().unwrap(),
                "alpha" => s.alpha = value.get().unwrap(),
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let s = self.settings.lock().unwrap();
            match pspec.name() {
                "source" => s.source.to_value(),
                "quality" => s.quality.to_value(),
                "alpha" => s.alpha.to_value(),
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

    impl GstObjectImpl for OmtVideoSrc {}

    impl ElementImpl for OmtVideoSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: std::sync::LazyLock<gst::subclass::ElementMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "OMT Video Source",
                        "Source/Network/Video",
                        "Receives video from an Open Media Transport source",
                        "Meros <https://meros.co>",
                    )
                });
            Some(&META)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::LazyLock<Vec<gst::PadTemplate>> =
                std::sync::LazyLock::new(|| {
                    let caps = gst_video::VideoCapsBuilder::new()
                        .format_list([VideoFormat::Uyvy, VideoFormat::Bgra])
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

    impl BaseSrcImpl for OmtVideoSrc {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let s = self.settings.lock().unwrap().clone();
            if s.source.is_empty() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["no source set"]
                ));
            }
            let options = ReceiverOptions {
                video: true,
                audio: false,
                quality: s.quality.request(),
            };
            let receiver = BlockingReceiver::connect(&s.source, options).map_err(|e| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["OMT source {}: {}", s.source, e]
                )
            })?;
            // Short reads so a flush or stop is noticed promptly.
            receiver
                .set_read_timeout(Some(Duration::from_millis(100)))
                .map_err(|e| gst::error_msg!(gst::ResourceError::OpenRead, ["{}", e]))?;
            gst::info!(
                CAT,
                imp = self,
                "connected to {} ({})",
                s.source,
                receiver.address()
            );
            self.time_base().reset();
            *self.running.lock().unwrap() = Some(Running {
                receiver,
                decoder: VmxDecoder::new(),
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

    impl PushSrcImpl for OmtVideoSrc {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
            let alpha = self.settings.lock().unwrap().alpha;
            let (format, decode) = if alpha {
                (VideoFormat::Bgra, DecodeFormat::Bgra)
            } else {
                (VideoFormat::Uyvy, DecodeFormat::Uyvy)
            };
            let mut guard = self.running.lock().unwrap();
            let running = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            let video = loop {
                if self.flushing.load(Ordering::SeqCst) {
                    return Err(gst::FlowError::Flushing);
                }
                match running.receiver.next_frame() {
                    Ok(Frame::Video(v)) => break v,
                    Ok(_) => continue,
                    Err(omt::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(omt::Error::Disconnected(reason)) => {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Read,
                            ["OMT source disconnected: {}", reason]
                        );
                        return Err(gst::FlowError::Error);
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

            let h = &video.header;
            let (w, ht) = (h.width, h.height);
            let key = (w, ht, h.frame_rate_n, h.frame_rate_d.max(1), format);
            if running.caps_for != Some(key) {
                let info = gst_video::VideoInfo::builder(format, w as u32, ht as u32)
                    .fps(gst::Fraction::new(h.frame_rate_n, h.frame_rate_d.max(1)))
                    .build()
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                let caps = info.to_caps().map_err(|_| gst::FlowError::NotNegotiated)?;
                drop(guard);
                self.obj()
                    .set_caps(&caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                guard = self.running.lock().unwrap();
            }
            let running = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            running.caps_for = Some(key);

            let size = decode.frame_size(w, ht);
            let mut buffer = gst::Buffer::with_size(size).map_err(|_| gst::FlowError::Error)?;
            {
                let buf = buffer.get_mut().unwrap();
                let mut map = buf.map_writable().map_err(|_| gst::FlowError::Error)?;
                running
                    .decoder
                    .decode(
                        &video.data,
                        w,
                        ht,
                        decode,
                        map.as_mut_slice(),
                        decode.stride(w),
                    )
                    .map_err(|e| {
                        gst::element_imp_error!(self, gst::StreamError::Decode, ["{}", e]);
                        gst::FlowError::Error
                    })?;
            }
            drop(guard);
            let pts = self
                .time_base()
                .running_time(video.timestamp, self.running_time_now());
            {
                let buf = buffer.get_mut().unwrap();
                buf.set_pts(pts);
                if h.frame_rate_n > 0 {
                    buf.set_duration(
                        gst::ClockTime::SECOND
                            .mul_div_floor(h.frame_rate_d.max(1) as u64, h.frame_rate_n as u64),
                    );
                }
            }
            Ok(CreateSuccess::NewBuffer(buffer))
        }
    }
}

glib::wrapper! {
    pub struct OmtVideoSrc(ObjectSubclass<imp::OmtVideoSrc>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl OmtVideoSrc {
    pub(crate) fn set_time_base(&self, time_base: std::sync::Arc<crate::shared::TimeBase>) {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        self.imp().set_time_base(time_base);
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtvideosrc",
        gst::Rank::NONE,
        OmtVideoSrc::static_type(),
    )
}
