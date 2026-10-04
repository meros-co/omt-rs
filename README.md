# omt-rs

[Open Media Transport](https://openmediatransport.org) (OMT) in Rust: send,
receive, discovery and the VMX codec, on Windows, macOS, Linux, Android and
iOS.

- **`omt-rs`** (`crates/omt-rs`, imported as `omt`) — the library. The protocol
  is pure Rust; the VMX video codec is the official MIT-licensed libvmx,
  vendored and compiled by `build.rs`, so there is nothing to install and no
  .NET runtime.

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

`tests/interop.rs` checks both directions against the official libomt and runs
in CI on all three desktop platforms.

## Tests

```sh
cargo test                              # unit + sender-to-receiver over loopback
OMT_TEST_MDNS=1 cargo test              # + real mDNS advertise/discover
scripts/build-libomt.sh                 # (or .ps1 on Windows) build the reference
OMT_LIB_DIR=target/libomt-ref cargo test --features interop --test interop
```

## Licence

MIT. The vendored libvmx and sse2neon are MIT too; ship
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) with binaries.
