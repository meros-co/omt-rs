//! GStreamer elements for Open Media Transport, built on the `omt-rs` crate.
//!
//! | Element         | Kind                   | What it does                                    |
//! |-----------------|------------------------|-------------------------------------------------|
//! | `omtsrc`        | bin, `video`/`audio` request pads | receive an OMT source (video and/or audio) |
//! | `omtsink`       | bin, `video`/`audio` request pads | publish an OMT source                      |
//! | `omtvideosrc`   | live push source       | receive video only                              |
//! | `omtaudiosrc`   | live push source       | receive audio only                              |
//! | `omtvideosink`  | sink                   | send video                                      |
//! | `omtaudiosink`  | sink                   | send audio                                      |
//! | `omtdeviceprovider` | device provider    | list OMT sources on the network                 |
//!
//! OMT receivers already use one connection per media type, so the video and
//! audio elements each run their own; the bins put them back together, with
//! one shared, drift-corrected timeline so audio and video stay in sync, and
//! one set of tally, sender info and statistics (see `shared` for the bus
//! messages).
//!
//! ```text
//! gst-launch-1.0 omtsrc source="STUDIO (Program)" name=s \
//!     s.video ! videoconvert ! autovideosink \
//!     s.audio ! audioconvert ! autoaudiosink
//! gst-launch-1.0 videotestsrc is-live=true ! video/x-raw,format=UYVY ! omtsink omt-name=Test
//! ```

use gst::glib;

mod audiosink;
mod audiosrc;
mod deviceprovider;
mod shared;
mod sinkbin;
mod sinkprops;
mod srcbin;
mod srcprops;
mod videosink;
mod videosrc;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    videosrc::register(plugin)?;
    audiosrc::register(plugin)?;
    srcbin::register(plugin)?;
    videosink::register(plugin)?;
    audiosink::register(plugin)?;
    sinkbin::register(plugin)?;
    deviceprovider::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    omt,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
