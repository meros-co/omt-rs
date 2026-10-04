//! `omtsink`: one OMT source carrying video and/or audio. Request a `video`
//! pad, an `audio` pad, or both; each is backed by an `omtvideosink` /
//! `omtaudiosink` sharing one sender.

use gst::glib;
use gst::prelude::*;

mod imp {
    use std::sync::Mutex;

    use gst::subclass::prelude::*;

    use super::*;
    use crate::shared::CAT;
    use crate::videosink::{SinkSettings, set_sink_property, sink_properties, sink_property};

    #[derive(Default)]
    pub struct OmtSink {
        settings: Mutex<SinkSettings>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OmtSink {
        const NAME: &'static str = "GstOmtSink";
        type Type = super::OmtSink;
        type ParentType = gst::Bin;
    }

    impl ObjectImpl for OmtSink {
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
    }

    impl GstObjectImpl for OmtSink {}

    impl ElementImpl for OmtSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: std::sync::LazyLock<gst::subclass::ElementMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "OMT Sink",
                        "Sink/Network/Video/Audio",
                        "Publishes video and audio as one Open Media Transport source",
                        "Meros <https://meros.co>",
                    )
                });
            Some(&META)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::LazyLock<Vec<gst::PadTemplate>> =
                std::sync::LazyLock::new(|| {
                    let template = |name: &str, factory: &str| {
                        let caps = child_template_caps(factory);
                        gst::PadTemplate::new(
                            name,
                            gst::PadDirection::Sink,
                            gst::PadPresence::Request,
                            &caps,
                        )
                        .unwrap()
                    };
                    vec![
                        template("video", "omtvideosink"),
                        template("audio", "omtaudiosink"),
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
            let factory = if pad_name == "video" {
                "omtvideosink"
            } else {
                "omtaudiosink"
            };
            let s = self.settings.lock().unwrap().clone();
            let child = gst::ElementFactory::make(factory)
                .property("omt-name", &s.omt_name)
                .property("port", s.port)
                .property("quality", s.quality)
                .property("advertise", s.advertise)
                .build()
                .ok()?;
            obj.add(&child).ok()?;
            let target = child.static_pad("sink")?;
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

    impl BinImpl for OmtSink {}

    pub(crate) fn child_template_caps(factory: &str) -> gst::Caps {
        gst::ElementFactory::find(factory)
            .and_then(|f| {
                f.static_pad_templates()
                    .into_iter()
                    .next()
                    .map(|t| t.caps())
            })
            .unwrap_or_else(gst::Caps::new_any)
    }
}

pub(crate) use imp::child_template_caps;

glib::wrapper! {
    pub struct OmtSink(ObjectSubclass<imp::OmtSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "omtsink",
        gst::Rank::NONE,
        OmtSink::static_type(),
    )
}
