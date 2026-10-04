//! Compiles the vendored VMX codec (`vendor/libvmx`, MIT) for the target.
//!
//! Per target, mirroring upstream's own build scripts (vendor/libvmx/build):
//!
//! | Target            | Sources                          | Notes                          |
//! |-------------------|----------------------------------|--------------------------------|
//! | aarch64 (any OS)  | vmxcodec + vmxcodec_arm          | NEON via sse2neon               |
//! | armv7 (Android)   | vmxcodec + vmxcodec_arm          | NEON path forced with `ARM64`   |
//! | x86_64 / x86      | vmxcodec + vmxcodec_x86, plus    | AVX2 confined to its own unit;  |
//! |                   | vmxcodec_avx2 in its own archive | libvmx picks it at runtime      |
//!
//! libvmx dispatches to its AVX2 code with `__cpuidex` at runtime, so AVX2 must
//! stay confined to `vmxcodec_avx2.cpp` (as upstream's `VMXCodec.vcxproj`
//! does); widening it to every unit would emit AVX2 into code that runs on CPUs
//! without it. `cc::Build` carries one flag set per invocation, hence two
//! archives on x86.
//!
//! libvmx uses `__declspec(align(...))`, so non-MSVC compilers need
//! `-fdeclspec`, which only clang accepts. Upstream builds Linux with clang++
//! for the same reason; we prefer it too unless `CXX` says otherwise.

use std::path::PathBuf;

fn main() {
    let src = vmx_src();
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-env-changed=CXX");

    let target_os = env("CARGO_CFG_TARGET_OS");
    let arch = env("CARGO_CFG_TARGET_ARCH");
    let msvc = env("CARGO_CFG_TARGET_ENV") == "msvc";
    let arm = arch == "aarch64" || arch == "arm";

    let mut core = base(msvc, &target_os);
    core.file(src.join("vmxcodec.cpp"));
    if arm {
        core.file(src.join("vmxcodec_arm.cpp"));
        if arch == "arm" {
            // armv7: upstream gates the NEON path on __aarch64__, but its
            // sse2neon layer supports ARMv7-A with NEON. Select it explicitly.
            core.define("ARM64", None).flag("-mfpu=neon");
        }
    } else {
        core.file(src.join("vmxcodec_x86.cpp"))
            // Upstream's baseline: every x64 CPU since Sandy Bridge has AVX.
            .flag(if msvc { "/arch:AVX" } else { "-mavx" });
    }
    core.compile("vmx");

    if !arm {
        let mut avx2 = base(msvc, &target_os);
        avx2.file(src.join("vmxcodec_avx2.cpp"));
        if msvc {
            avx2.flag("/arch:AVX2");
        } else {
            avx2.flag("-mavx2").flag("-mlzcnt").flag("-mbmi");
        }
        avx2.compile("vmx_avx2");
    }
}

fn base(msvc: bool, target_os: &str) -> cc::Build {
    let mut b = cc::Build::new();
    b.cpp(true).std("c++17").opt_level(3);
    if !msvc {
        b.flag("-fdeclspec");
        // GCC rejects -fdeclspec; use clang on Linux unless CXX is set
        // explicitly (Android/Apple toolchains are clang already).
        if target_os == "linux" && std::env::var_os("CXX").is_none() {
            b.compiler("clang++");
        }
    }
    b
}

/// The vendored libvmx sources, at the workspace root. A git dependency
/// checks out the whole repository, so the path holds there too.
fn vmx_src() -> PathBuf {
    PathBuf::from(env("CARGO_MANIFEST_DIR")).join("../../vendor/libvmx/src")
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}
