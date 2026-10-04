# Vendored: libvmx (VMX codec)

- Upstream: https://github.com/openmediatransport/libvmx
- Pinned commit: `f73569e767b9d9177519bf5765c9434dfe8af51f` (2026-04-10,
  "Fix encode issue with NV12 and P216 sources where stride is unaligned…")
- License: MIT (see LICENSE in this directory)

This is a vendored copy (not a submodule) so builds are reproducible and
offline-safe. Compiled for every target (Android, iOS, macOS, Windows, Linux)
by `crates/omt-rs/build.rs` via the `cc` crate; FFI wrapper at
`crates/omt-rs/src/vmx.rs`. Moved here from DrawLive (`rust/vendor/libvmx`)
when its OMT code became omt-rs.

## Local patches

Two, kept as small as possible so an upstream refresh is easy to re-apply:

- `src/vmxcodec.h` — the `_MSC_VER` arm of the `VMX_API` macro gained
  `extern "C"`. Upstream defines it as bare `__declspec(dllexport)` while the
  GCC/Clang arm directly below it says `extern "C" __attribute__(...)`, so
  under MSVC every entry point is C++-name-mangled and the Rust `extern "C"`
  declarations in `vmx.rs` fail to link (`unresolved external symbol
  VMX_Create`). Upstream consumes the library from C++ via this same header,
  which is why the asymmetry went unnoticed there.
- `src/thread_tasks.h` — both condition-variable waits gained predicates.
  `TaskLoop` read `running` outside the lock and then waited, so a
  `Destroy` landing in between lost its wake-up and `thread.join()` hung
  forever (seen as `VMX_SetThreads` deadlocking when a sender starts while
  other encoders run); `Join` could also return on a spurious wake-up.

To update: clone upstream at the desired commit, replace this directory's
contents (keep this file), update the pinned commit above, and re-run the
Rust tests, including the interop suite against the official libomt.
