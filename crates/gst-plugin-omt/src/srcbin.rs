//! `omtsrc`: receives an OMT source's video and/or audio. Request a `video`
//! pad, an `audio` pad, or both; each is backed by an `omtvideosrc` /
//! `omtaudiosrc` sharing one drift-corrected timeline, tally, sender info and
//! statistics.

use gst::glib;
use gst::prelude::*;

mod imp {
    use std::sync::{Arc, Mutex};

    use gst::subclass::prelude::*;

    use super::*;
    use crate::audiosrc::OmtAudioSrc;
    use crate::shared::{CAT, SourceShared};
    use crate::srcprops::{self, SrcSettings};
    use crate::videosrc::OmtVideoSrc;

    pub struct OmtSrc {
        settings: Mutex<SrcSettings>,
        shared: Arc<SourceShared>,
    }

    impl Default for OmtSrc {
        fn default() -> Self {
            Self {
                settings: Mutex::default(),
                shared: SourceShared::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OmtSrc {
        const NAME: &'static str = "GstOmtSrc";
        type Type = super::OmtSrc;
        type ParentType = gst::Bin;
    }

    impl ObjectImpl for OmtSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: std::sync::LazyLock<Vec<glib::ParamSpec>> =
                std::sync::LazyLock::new(|| srcprops::properties(true));
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            srcprops::set(
                &mut self.settings.lock().unwrap(),
                &self.shared,
                value,
                pspec,
            );
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            srcprops::get(&self.settings.lock().unwrap(), &self.shared, pspec)
        }
    }

    impl GstObjectImpl for OmtSrc {}

    impl ElementImpl for OmtSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: std::sync::LazyLock<gst::subclass::ElementMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "OMT Source",
                        "Source/Network/Video/Audio",
                        "Receives video and audio from an Open Media Transport source",
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
                            "video",
                            gst::PadDirection::Src,
                            gst::PadPresence::Request,
                            &crate::videosrc::caps(),
                        )
                        .unwrap(),
                        gst::PadTemplate::new(
                            "audio",
                            gst::PadDirection::Src,
                            gst::PadPresence::Request,
                            &crate::audiosrc::caps(),
                        )
                        .unwrap(),
                    ]
                });
            TEMPLATES.as_ref()
        }

        fn request_new_pad(
            &self,
            templ: &gst::PadTemplate,
            _name: Option<&str>,
            _caps: Option<&gst::Caps>,
        ) -> Option<gst::Pad> {
            let obj = self.obj();
            let pad_name = templ.name_template().to_string();
            if obj.static_pad(&pad_name).is_some() {
                gst::warning!(CAT, imp = self, "{pad_name} pad already requested");
                return None;
            }
            let s = self.settings.lock().unwrap().clone();
            let child: gst::Element = if pad_name == "video" {
                let src: OmtVideoSrc = glib::Object::builder()
                    .property("source", &s.source)
                    .property("quality", s.quality)
                    .property("alpha", s.alpha)
                    .property("latency", s.latency_ms)
                    .build();
                src.set_shared(self.shared.clone());
                src.upcast()
            } else {
                let src: OmtAudioSrc = glib::Object::builder()
                    .property("source", &s.source)
                    .property("latency", s.latency_ms)
                    .build();
                src.set_shared(self.shared.clone());
                src.upcast()
            };
            obj.add(&child).ok()?;
            let target = child.static_pad("src")?;
            let ghost = gst::GhostPad::builder_from_template_with_target(templ, &target)
                .ok()?
                .name(&pad_name)
                .build();
            ghost.set_active(true).ok()?;
            obj.add_pad(&ghost).ok()?;
            child.sync_state_with_parent().ok()?;
            Some(ghost.upcast())
        }

        fn release_pad(&self, pad: &gst::Pad) {
            let obj = self.obj();
            if let Some(ghost) = pad.downcast_ref::<gst::GhostPad>() {
                if let Some(child) = ghost.target().and_then(|t| t.parent_element()) {
                    let _ = child.set_state(gst::State::Null);
                    let _ = obj.remove(&child);
                }
            }
            let _ = obj.remove_pad(pad);
        }
    }

    impl BinImpl for OmtSrc {}
}

glib::wrapper! {
    pub struct OmtSrc(ObjectSubclass<imp::OmtSrc>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtsrc",
        gst::Rank::NONE,
        OmtSrc::static_type(),
    )
}
