# omt-rs

[Open Media Transport](https://openmediatransport.org) (OMT) in Rust: send,
receive, discovery and the VMX codec, on Windows, macOS, Linux, Android and
iOS.

- **`omt-rs`** (`crates/omt-rs`, imported as `omt`) — the library. The protocol
  is pure Rust; the VMX video codec is the official MIT-licensed libvmx,
  vendored and compiled by `build.rs`, so there is nothing to install and no
  .NET runtime.
- **`gst-plugin-omt`** (`crates/gst-plugin-omt`) — GStreamer elements:
  `omtsrc`, `omtsink` and an OMT device provider (see below).

The crate is called `omt-rs` because `omt` and `libomt` on crates.io belong to
other projects (`libomt` wraps the official .NET-based library, which has no
Android or iOS build).

## Use

```toml
[dependencies]
omt-rs = { git = "https://github.com/meros-co/omt-rs" }
```

Send:

```rust
use omt::{PixelFormat, Sender, SenderConfig};

let mut sender = Sender::new(SenderConfig::new("Program", 1920, 1080, (60, 1)))?;
// per frame; timestamps are OMT ticks (100 ns), shared by video and audio
sender.send_video(PixelFormat::Uyvy, &mut uyvy, 1920 * 2, timestamp)?;
sender.send_audio(48_000, 2, &planar_f32, timestamp)?;
```

Receive (blocking; `omt::Receiver` is the tokio equivalent):

```rust
use omt::{BlockingReceiver, Frame, ReceiverOptions};
use omt::vmx::{DecodeFormat, VmxDecoder};

let mut rx = BlockingReceiver::connect("STUDIO (Program)", ReceiverOptions::default())?;
let mut decoder = VmxDecoder::new();
loop {
    match rx.next_frame()? {
        Frame::Video(v) => {
            let (w, h) = (v.header.width, v.header.height);
            let mut uyvy = vec![0; DecodeFormat::Uyvy.frame_size(w, h)];
            decoder.decode(&v.data, w, h, DecodeFormat::Uyvy, &mut uyvy, w * 2)?;
        }
        Frame::Audio(a) => { /* a.planar: f32, planar */ }
        Frame::Metadata { .. } => {}
    }
}
```

Discover: `omt::discovery::discover(timeout_ms)` lists `MACHINE (Name)`
sources; `resolve` turns one into `host:port`. On a PC with several adapters
(Hyper-V, WSL, Docker), `omt::net::set_preferred_interface` pins listening,
advertising and browsing to one address.

Tally, sender info and statistics (`omt::metadata`):

```rust
// Sender: say who you are; hear tally back from every receiver, combined.
sender.set_sender_info(Some(&omt::SenderInfo {
    product_name: "Helm".into(), manufacturer: "Meros".into(), version: "1.0".into(),
}));
sender.on_tally_changed(|t| println!("program={} preview={}", t.program, t.preview));
let stats = sender.statistics(); // connections, frames, drops, bytes sent

// Receiver: subscribe to metadata, set your tally, read the source's info.
let mut rx = BlockingReceiver::connect(name, ReceiverOptions { metadata: true, ..Default::default() })?;
rx.control()?.send_tally(omt::Tally { program: true, preview: false })?;
let info = rx.sender_info();     // once received (sent on connect)
let combined = rx.sender_tally(); // every receiver's tally, as the sender reports it
```

Clock recovery (`omt::sync`): `ClockRecovery` maps a sender's timestamps onto
your clock and corrects the drift between them (a phase-locked loop on
arrival times, using each second's minimum delay so network jitter does not
move it); `DriftResampler` stretches audio by the measured ppm so it stays
continuous on that timeline. Shared by a source's video and audio, they keep
the two in sync with each other and with the local clock indefinitely — the
tests simulate eight hours at up to 300 ppm and hold A/V within a frame.

## GStreamer

```sh
cargo build -p gst-plugin-omt --release      # needs GStreamer's development files
export GST_PLUGIN_PATH=$PWD/target/release   # gstomt.dll / libgstomt.so / .dylib
gst-inspect-1.0 omt
```

| Element | |
|---|---|
| `omtsrc` | receives a source; request `video` and/or `audio` pads |
| `omtsink` | publishes a source; request `video` and/or `audio` pads |
| `omtvideosrc`, `omtaudiosrc` | the single-media sources `omtsrc` is built from |
| `omtvideosink`, `omtaudiosink` | the single-media sinks `omtsink` is built from (same `omt-name` = one source) |
| `omtdeviceprovider` | lists sources on the network through `GstDeviceMonitor` |

```sh
gst-launch-1.0 omtsrc source="STUDIO (Program)" name=s \
    s.video ! videoconvert ! autovideosink \
    s.audio ! audioconvert ! autoaudiosink

gst-launch-1.0 videotestsrc is-live=true ! video/x-raw,format=UYVY ! s.video \
    audiotestsrc is-live=true ! audio/x-raw,format=F32LE,rate=48000 ! s.audio \
    omtsink name=s omt-name=Test
```

- `source` is a discovered name (`MACHINE (Name)`) or `host:port`.
- Video comes out as UYVY, or BGRA with `alpha=true`; sinks take UYVY, BGRA
  or BGRx. Audio is F32 (sinks take interleaved or non-interleaved).
- `omtsink` properties: `omt-name`, `port` (0 = 6960 or any free port),
  `quality` (`low`/`standard`/`high`, the VMX bitrate tier at the same
  resolution) and `advertise`.
- Timestamps and drift: `omtsrc` maps the sender's timestamps onto the
  pipeline clock through one clock-recovery loop shared by its video and
  audio, so the two stay in sync, and the drift between the sender's clock
  and the pipeline's is corrected rather than accumulated (audio is resampled
  by the measured ppm; video timestamps follow the corrected timeline, so
  sinks drop or repeat frames as needed). `drift-correction=false` turns it
  off. `latency` (default 50 ms) is what the source reports for jitter and
  decoding.

### Tally, sender info and statistics

| | `omtsrc` (and `omtvideosrc` / `omtaudiosrc`) | `omtsink` (and its sinks) |
|---|---|---|
| Tally | set `tally-program` / `tally-preview` to tell the source | read `tally-program` / `tally-preview`: any receiver has it on program / preview |
| Sender info | read `sender-product-name`, `sender-manufacturer`, `sender-version` | set `product-name`, `manufacturer`, `version` |
| Statistics | `stats` property | `stats` property |

Bus messages (element messages from the `omtsrc` / `omtsink`, or from the
single element used on its own):

- `omt-stats`, every `stats-interval` ms (default 1000; 0 = off).
  Source: `connected`, `video-frames`, `audio-frames`, `video-dropped`
  (frames missing from the sender's timeline), `bytes-received`, `bitrate`
  (bits/s), `drift-ppm` (settled clock drift, sender vs pipeline),
  `correction-ppm` (the rate correction applied now), `phase-error-ms`,
  `jitter-ms` (arrival jitter beyond the minimum delay), `tally-program` /
  `tally-preview` (ours) and `source-tally-program` / `source-tally-preview`
  (the combined tally the sender reports). Sink: `connected`, `connections`,
  `video-frames`, `audio-frames`, `video-dropped` / `audio-dropped` (frames a
  slow receiver missed), `bytes-sent`, `bitrate`, `tally-program`,
  `tally-preview`.
- `omt-tally` when the combined tally changes: `program`, `preview`.
- `omt-sender-info` on `omtsrc` when the source's info arrives:
  `product-name`, `manufacturer`, `version`.

`omtdeviceprovider` devices carry `omt.source` and, when the source has set
them, `omt.product-name`, `omt.manufacturer` and `omt.version` in their
properties.

## Compatibility with the official libomt

Where the OMT spec text and the reference implementation disagree, omt-rs
follows the reference, because that is what is on the wire:

1. **Metadata frames carry raw UTF-8 with no null terminator** and a header
   `MetadataLength` of 0. The spec says "including null character"; libomt
   compares subscribe commands exactly, so a null makes it ignore the
   subscribe and send nothing.
2. **Each connection gets only the frame types it subscribed to.** libomt
   opens separate video-only and audio-only connections per receiver; sending
   every type everywhere scrambles its video ordering and starves its audio.
3. **Discovery prefers IPv4.** mDNS answers often include link-local IPv6,
   which cannot be dialled without a scope id.
4. **Source names are `MACHINE (Name)`** — libomt discards records without the
   parentheses.
5. **Tally strings keep libomt's typo.** `<OMTTally Preview="true" Program=="false" />`
   — the double `==` is in libomt and compared exactly, so omt-rs writes it
   too. A sender ORs every connection's tally, sends its sender info and the
   current tally to each new connection, and broadcasts tally changes to
   connections subscribed to metadata, as libomt does.

`tests/interop.rs` checks media, tally and sender info in both directions
against the official libomt and runs in CI on all three desktop platforms.

## Tests

```sh
cargo test                              # library: unit + sender-to-receiver over loopback
cargo test -p gst-plugin-omt            # elements against each other
OMT_TEST_MDNS=1 cargo test              # + real mDNS advertise/discover
scripts/build-libomt.sh                 # (or .ps1 on Windows) build the reference
OMT_LIB_DIR=target/libomt-ref cargo test --features interop --test interop
```

## Licence

MIT. The vendored libvmx and sse2neon are MIT too; ship
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) with binaries. The GStreamer
plugin links GStreamer (LGPL) dynamically.
