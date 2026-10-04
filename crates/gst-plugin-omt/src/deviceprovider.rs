//! `omtdeviceprovider`: lists OMT sources on the network as GStreamer devices,
//! so applications find them through `GstDeviceMonitor` like any camera.

use gst::glib;
use gst::prelude::*;

mod imp {
    use std::sync::OnceLock;

    use gst::subclass::prelude::*;

    use super::*;

    /// How long one probe browses mDNS.
    const PROBE_MS: u64 = 1500;

    #[derive(Default)]
    pub struct OmtDeviceProvider;

    #[glib::object_subclass]
    impl ObjectSubclass for OmtDeviceProvider {
        const NAME: &'static str = "GstOmtDeviceProvider";
        type Type = super::OmtDeviceProvider;
        type ParentType = gst::DeviceProvider;
    }

    impl ObjectImpl for OmtDeviceProvider {}
    impl GstObjectImpl for OmtDeviceProvider {}

    impl DeviceProviderImpl for OmtDeviceProvider {
        fn metadata() -> Option<&'static gst::subclass::DeviceProviderMetadata> {
            static META: std::sync::LazyLock<gst::subclass::DeviceProviderMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::DeviceProviderMetadata::new(
                        "OMT Device Provider",
                        "Source/Network/Video/Audio",
                        "Lists Open Media Transport sources on the network",
                        "Meros <https://meros.co>",
                    )
                });
            Some(&META)
        }

        fn probe(&self) -> Vec<gst::Device> {
            omt::discovery::discover(PROBE_MS)
                .into_iter()
                .map(|name| super::OmtDevice::new(&name).upcast())
                .collect()
        }
    }

    #[derive(Default)]
    pub struct OmtDevice {
        pub(super) source: OnceLock<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OmtDevice {
        const NAME: &'static str = "GstOmtDevice";
        type Type = super::OmtDevice;
        type ParentType = gst::Device;
    }

    impl ObjectImpl for OmtDevice {}
    impl GstObjectImpl for OmtDevice {}

    impl DeviceImpl for OmtDevice {
        fn create_element(&self, name: Option<&str>) -> Result<gst::Element, gst::LoggableError> {
            let source = self.source.get().cloned().unwrap_or_default();
            let mut builder = gst::ElementFactory::make("omtsrc").property("source", source);
            if let Some(name) = name {
                builder = builder.name(name);
            }
            builder
                .build()
                .map_err(|e| gst::loggable_error!(crate::shared::CAT, "creating omtsrc: {e}"))
        }
    }
}

glib::wrapper! {
    pub struct OmtDeviceProvider(ObjectSubclass<imp::OmtDeviceProvider>) @extends gst::DeviceProvider, gst::Object;
}

glib::wrapper! {
    pub struct OmtDevice(ObjectSubclass<imp::OmtDevice>) @extends gst::Device, gst::Object;
}

impl OmtDevice {
    fn new(source: &str) -> Self {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        let caps = gst::Caps::builder("video/x-raw").build();
        let props = gst::Structure::builder("properties")
            .field("omt.source", source)
            .build();
        let device: Self = glib::Object::builder()
            .property("display-name", source)
            .property("device-class", "Source/Video")
            .property("caps", &caps)
            .property("properties", &props)
            .build();
        let _ = device.imp().source.set(source.to_string());
        device
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::DeviceProvider::register(
        Some(plugin),
        "omtdeviceprovider",
        gst::Rank::PRIMARY,
        OmtDeviceProvider::static_type(),
    )
}
