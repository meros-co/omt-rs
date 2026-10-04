//! `omtaudiosrc`: receives an OMT source's audio as interleaved F32, resampled
//! by the measured clock drift so it stays continuous on the corrected
//! timeline it shares with the video.

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
    use crate::shared::{CAT, SourceShared, running_time_now};
    use crate::srcprops::{self, SrcSettings};
    use omt::receive::{BlockingReceiver, Frame, ReceiverOptions};
    use omt::sync::DriftResampler;

    struct Running {
        receiver: BlockingReceiver,
        resampler: DriftResampler,
        caps_for: Option<(i32, i32)>,
    }

    pub struct OmtAudioSrc {
        settings: Mutex<SrcSettings>,
        shared: Mutex<Arc<SourceShared>>,
        running: Mutex<Option<Running>>,
        flushing: AtomicBool,
    }

    impl Default for OmtAudioSrc {
        fn default() -> Self {
            Self {
                settings: Mutex::default(),
                shared: Mutex::new(SourceShared::new()),
                running: Mutex::default(),
                flushing: AtomicBool::new(false),
            }
        }
    }

    impl OmtAudioSrc {
        pub fn set_shared(&self, shared: Arc<SourceShared>) {
            *self.shared.lock().unwrap() = shared;
        }

        fn shared(&self) -> Arc<SourceShared> {
            self.shared.lock().unwrap().clone()
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
                std::sync::LazyLock::new(|| srcprops::properties(false));
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

    impl BaseSrcImpl for OmtAudioSrc {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let source = self.settings.lock().unwrap().source.clone();
            if source.is_empty() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["no source set"]
                ));
            }
            // With no video sibling, this connection carries the metadata.
            let shared = self.shared();
            let options = ReceiverOptions {
                video: false,
                audio: true,
                metadata: true,
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
            shared.claim_owner(self.obj().upcast_ref());
            if let Ok(control) = receiver.control() {
                shared.connected(control);
            }
            *self.running.lock().unwrap() = Some(Running {
                receiver,
                resampler: DriftResampler::new(),
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

        /// Caps come from the stream; see `omtvideosrc`.
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

    impl PushSrcImpl for OmtAudioSrc {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
            let shared = self.shared();
            let mut guard = self.running.lock().unwrap();
            let running = guard.as_mut().ok_or(gst::FlowError::Flushing)?;
            let audio = loop {
                if self.flushing.load(Ordering::SeqCst) {
                    return Err(gst::FlowError::Flushing);
                }
                let frame = running.receiver.next_frame();
                shared.learn(&running.receiver);
                match frame {
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
            let arrival = running_time_now(self.obj().upcast_ref());
            let (rate, channels) = (audio.header.sample_rate, audio.header.channels.clamp(1, 32));
            shared.count_audio((audio.planar.len() * 4) as u64);
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
            let running = guard.as_mut().ok_or(gst::FlowError::Flushing)?;

            let out = if shared
                .drift_correction
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                // Resampled onto the shared, drift-corrected timeline.
                let mut clock = shared.clock.lock().unwrap();
                running.resampler.process(
                    &mut clock,
                    &audio.planar,
                    rate,
                    channels as usize,
                    audio.timestamp,
                    arrival.nseconds() as i64,
                )
            } else {
                // Uncorrected: interleave as received, fixed-offset timestamps.
                let ch = channels as usize;
                let spc = audio.planar.len() / ch;
                let mut interleaved = vec![0f32; spc * ch];
                for i in 0..spc {
                    for c in 0..ch {
                        interleaved[i * ch + c] = audio.planar[c * spc + i];
                    }
                }
                omt::sync::ResampledAudio {
                    pts_ns: shared.running_time(audio.timestamp, arrival).nseconds() as i64,
                    interleaved,
                    frames: spc,
                    discont: false,
                }
            };
            drop(guard);
            if out.frames == 0 {
                // The resampler absorbed this buffer entirely; read the next.
                return PushSrcImpl::create(self, None);
            }

            let mut buffer = gst::Buffer::with_size(out.interleaved.len() * 4)
                .map_err(|_| gst::FlowError::Error)?;
            {
                let buf = buffer.get_mut().unwrap();
                {
                    let mut map = buf.map_writable().map_err(|_| gst::FlowError::Error)?;
                    for (dst, s) in map.as_mut_slice().chunks_exact_mut(4).zip(&out.interleaved) {
                        dst.copy_from_slice(&s.to_le_bytes());
                    }
                }
                buf.set_pts(gst::ClockTime::from_nseconds(out.pts_ns.max(0) as u64));
                buf.set_duration(
                    gst::ClockTime::SECOND.mul_div_floor(out.frames as u64, rate.max(1) as u64),
                );
                if out.discont {
                    buf.set_flags(gst::BufferFlags::DISCONT);
                }
            }
            Ok(CreateSuccess::NewBuffer(buffer))
        }
    }
}

glib::wrapper! {
    pub struct OmtAudioSrc(ObjectSubclass<imp::OmtAudioSrc>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl OmtAudioSrc {
    pub(crate) fn set_shared(&self, shared: std::sync::Arc<crate::shared::SourceShared>) {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        self.imp().set_shared(shared);
    }
}

pub(crate) fn caps() -> gst::Caps {
    gst_audio::AudioCapsBuilder::new_interleaved()
        .format(gst_audio::AudioFormat::F32le)
        .channels_range(1..=32)
        .build()
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtaudiosrc",
        gst::Rank::NONE,
        OmtAudioSrc::static_type(),
    )
}
