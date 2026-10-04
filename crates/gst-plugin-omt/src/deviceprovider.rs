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
            // Sender info comes from the source itself (sent on connect), so
            // ask every source at once rather than one after another.
            let names = omt::discovery::discover(PROBE_MS);
            let lookups: Vec<_> = names
                .into_iter()
                .map(|name| {
                    std::thread::spawn(move || {
                        let info = sender_info(&name);
                        (name, info)
                    })
                })
                .collect();
            lookups
                .into_iter()
                .filter_map(|h| h.join().ok())
                .map(|(name, info)| super::OmtDevice::new(&name, info.as_ref()).upcast())
                .collect()
        }
    }

    /// How long to wait for a source to say who it is.
    const INFO_MS: u64 = 700;

    /// Connects to `name` without subscribing to anything and waits briefly
    /// for its sender info. `None` if it has none or does not answer.
    fn sender_info(name: &str) -> Option<omt::SenderInfo> {
        let options = omt::ReceiverOptions {
            video: false,
            audio: false,
            metadata: false,
            quality: None,
        };
        let mut rx = omt::BlockingReceiver::connect(name, options).ok()?;
        rx.set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .ok()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(INFO_MS);
        while std::time::Instant::now() < deadline {
            if let Some(info) = rx.sender_info() {
                return Some(info.clone());
            }
            if let Err(omt::Error::Disconnected(_)) = rx.next_frame() {
                break;
            }
        }
        rx.sender_info().cloned()
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
    fn new(source: &str, info: Option<&omt::SenderInfo>) -> Self {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        let caps = gst::Caps::builder("video/x-raw").build();
        let mut props = gst::Structure::builder("properties").field("omt.source", source);
        if let Some(info) = info {
            props = props
                .field("omt.product-name", &info.product_name)
                .field("omt.manufacturer", &info.manufacturer)
                .field("omt.version", &info.version);
        }
        let props = props.build();
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
