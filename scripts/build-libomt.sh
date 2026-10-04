#!/usr/bin/env bash
# Builds the official OMT libraries (libvmx, libomtnet, libomt) from source
# for the interop tests, on Linux or macOS. Upstream publishes no Linux
# binaries, so source is the only route there; it is used everywhere for one
# code path.
#
#   scripts/build-libomt.sh [out-dir]      (default: target/libomt-ref)
#   OMT_LIB_DIR=target/libomt-ref cargo test --features interop --test interop
#
# Needs git, the .NET 8 SDK (NativeAOT) and clang++.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$root/target/libomt-ref}"
work="$out/_src"
mkdir -p "$work" "$out"

for repo in libvmx libomtnet libomt; do
  if [ -d "$work/$repo/.git" ]; then
    git -C "$work/$repo" pull --ff-only
  else
    git clone --depth 1 "https://github.com/openmediatransport/$repo.git" "$work/$repo"
  fi
done

case "$(uname -s)-$(uname -m)" in
  Darwin-*)        script=buildmacuniversal.sh; ext=dylib; rid="" ;;
  Linux-aarch64)   script=buildlinuxarm64.sh;   ext=so;    rid=linux-arm64 ;;
  Linux-*)         script=buildlinuxx64.sh;     ext=so;    rid=linux-x64 ;;
  *) echo "unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# libomt's NativeAOT shim compiles against libomtnet, so it goes first.
(cd "$work/libomtnet" && dotnet build libomtnet.sln -c Release)
(cd "$work/libvmx/build" && bash "$script")
(cd "$work/libomt/build" && bash "$script")

cp "$work/libvmx/build/libvmx.$ext" "$out/"
if [ -n "$rid" ]; then
  cp "$work/libomt/bin/Release/net8.0/$rid/native/libomt.$ext" "$out/"
else
  cp "$work/libomt/build/libomt.$ext" "$out/"
fi
echo "libomt + libvmx in $out"
