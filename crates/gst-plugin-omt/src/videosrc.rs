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
    use crate::shared::{CAT, SourceShared, running_time_now};
    use crate::srcprops::{self, SrcSettings};
    use omt::receive::{BlockingReceiver, Frame, ReceiverOptions};
    use omt::vmx::{DecodeFormat, VmxDecoder};

    struct Running {
        receiver: BlockingReceiver,
        decoder: VmxDecoder,
        /// Width, height, framerate and format of the caps last set.
        caps_for: Option<(i32, i32, i32, i32, VideoFormat)>,
    }

    pub struct OmtVideoSrc {
        settings: Mutex<SrcSettings>,
        shared: Mutex<Arc<SourceShared>>,
        running: Mutex<Option<Running>>,
        flushing: AtomicBool,
    }

    impl Default for OmtVideoSrc {
        fn default() -> Self {
            Self {
                settings: Mutex::default(),
                shared: Mutex::new(SourceShared::new()),
                running: Mutex::default(),
                flushing: AtomicBool::new(false),
            }
        }
    }

    impl OmtVideoSrc {
        /// Shares state with a sibling audio source (set by `omtsrc`).
        pub fn set_shared(&self, shared: Arc<SourceShared>) {
            *self.shared.lock().unwrap() = shared;
        }

        fn shared(&self) -> Arc<SourceShared> {
            self.shared.lock().unwrap().clone()
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
                std::sync::LazyLock::new(|| srcprops::properties(true));
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            srcprops::set(
                &mut self.settings.lock().unwrap(),
                &self.shared(),
                value,
                pspec,
            );
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            srcprops::get(&self.settings.lock().unwrap(), &self.shared(), pspec)
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
                    vec![
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &super::caps(),
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
            // Metadata (tally broadcasts, sender info changes) on the video
            // connection, as libomt's receiver does.
            let options = ReceiverOptions {
                video: true,
                audio: false,
                metadata: true,
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
            let shared = self.shared();
            shared.claim_owner(self.obj().upcast_ref());
            if let Ok(control) = receiver.control() {
                shared.connected(control);
            }
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
                self.shared().disconnected();
            }
            Ok(())
        }

        fn is_seekable(&self) -> bool {
            false
        }

        /// Caps come from the stream (set on the first frame and on every
        /// format change). The default would fixate the template up front -
        /// a 1x1 picture - and force a renegotiation.
        fn negotiate(&self) -> Result<(), gst::LoggableError> {
            Ok(())
        }

        fn query(&self, query: &mut gst::QueryRef) -> bool {
            if let gst::QueryViewMut::Latency(_) = query.view_mut() {
                let ms = self.settings.lock().unwrap().latency_ms;
                return srcprops::latency_query(query, ms);
            }
            BaseSrcImplExt::parent_query(self, query)
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
            let shared = self.shared();
            let mut guard = self.running.lock().unwrap();
            let running = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            let video = loop {
                if self.flushing.load(Ordering::SeqCst) {
                    return Err(gst::FlowError::Flushing);
                }
                let frame = running.receiver.next_frame();
                shared.learn(&running.receiver);
                match frame {
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
            let arrival = running_time_now(self.obj().upcast_ref());

            let h = &video.header;
            let (w, ht) = (h.width, h.height);
            let (fps_n, fps_d) = (h.frame_rate_n, h.frame_rate_d.max(1));
            let frame_ticks = if fps_n > 0 {
                omt::protocol::TICKS_PER_SECOND * fps_d as i64 / fps_n as i64
            } else {
                0
            };
            shared.count_video(video.timestamp, frame_ticks, video.data.len() as u64);

            let key = (w, ht, fps_n, fps_d, format);
            if running.caps_for != Some(key) {
                let info = gst_video::VideoInfo::builder(format, w as u32, ht as u32)
                    .fps(gst::Fraction::new(fps_n, fps_d))
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
            let pts = shared.running_time(video.timestamp, arrival);
            {
                let buf = buffer.get_mut().unwrap();
                buf.set_pts(pts);
                if fps_n > 0 {
                    buf.set_duration(
                        gst::ClockTime::SECOND.mul_div_floor(fps_d as u64, fps_n as u64),
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
    pub(crate) fn set_shared(&self, shared: std::sync::Arc<crate::shared::SourceShared>) {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        self.imp().set_shared(shared);
    }
}

pub(crate) fn caps() -> gst::Caps {
    gst_video::VideoCapsBuilder::new()
        .format_list([gst_video::VideoFormat::Uyvy, gst_video::VideoFormat::Bgra])
        .build()
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtvideosrc",
        gst::Rank::NONE,
        OmtVideoSrc::static_type(),
    )
}
