#!/usr/bin/env bash
# Build one static FreeRDP — and the static OpenSSL it requires — with the claims about it
# verified rather than assumed.
#
# Usage:
#   ./build.sh <target>
#
# Targets:
#   macos-arm64      Apple silicon, deployment target from freerdp.env
#   linux-x86_64     x86-64 baseline
#   linux-aarch64    ARMv8-A baseline
#   windows-x86_64-msvc
#                    x86-64 baseline, MSVC, dynamic CRT — from MSYS2 bash inside a Visual Studio
#                    developer shell (see below)
#
# Output: dist/<target>/{lib,include}/… plus a MANIFEST naming both versions, both checksums, the
# full configure lines, the link order, and — measured rather than assumed — which system
# libraries and frameworks the archives need and which channels are compiled into them.
#
# **cmake, and FreeRDP's own build.** Which of its ~700 C files belong to which channel, which
# SIMD kernels compile on which architecture, and which of a hundred `WITH_*` options gate what,
# is knowledge that lives in FreeRDP's CMakeLists and nowhere else. Reproducing it here is the
# thing this repository exists to avoid: building it once, with FreeRDP's own build system, is
# exactly what frees every *consumer* from needing cmake, pkg-config, a C toolchain or OpenSSL.
#
# **Windows is MSVC, and it is the one target that is not a line in the case statement.** The
# OpenSSL half builds with `nmake` from a native (Strawberry) perl, FreeRDP with Ninja and `cl`,
# and the measurements below read COFF archives with LLVM's tools where the others use binutils.
# What stays the same is the shape: build both with their own build systems, then measure the
# result rather than describe it. The run needs MSYS2 bash (this script, `comm`, `awk`), a Visual
# Studio developer shell around it (`cl`, `link`, `nmake`, and `LIB`/`INCLUDE` for the SDK), nasm,
# a native perl, Ninja, and `llvm-nm`/`llvm-readobj`/`llvm-ar` on PATH. The archives are compiled
# against the dynamic CRT, because that is what Rust's `-msvc` target links, and OpenSSL's static
# libraries are `/Zl` — they name no CRT at all and take the consumer's — both asserted on the
# finished archives rather than trusted from the flags.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"
# shellcheck source=freerdp.env
. ./freerdp.env
# shellcheck source=source.sh
. ./source.sh

target="${1:-}"
[ -n "$target" ] || {
  sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d; s/^# \{0,1\}//'
  exit 1
}

out="$here/dist/$target"
work="$here/build/$target"
ssl_prefix="$work/openssl"

# Where a package manager keeps things this build must not find. `find_package(OpenSSL)` on a Mac
# with Homebrew installed picks up /opt/homebrew's OpenSSL — measured, not feared: an early build
# of this linked Homebrew's 3.6.3 while reporting success, which is exactly the unpinned,
# machine-dependent link this repository exists to prevent.
ignore_prefixes='/opt/homebrew;/usr/local;/home/linuxbrew/.linuxbrew'

windows=''
case "$target" in
  macos-arm64)
    openssl_target=darwin64-arm64-cc
    cpu_floor="armv8-a, macOS $MACOS_DEPLOYMENT_TARGET"
    ;;
  linux-x86_64)
    openssl_target=linux-x86_64
    # No `-march` floor. FreeRDP's SIMD (`libfreerdp/primitives/`) is dispatched through runtime
    # CPU detection — `primitives_init_*_opt` picks an implementation from what cpuid reports —
    # so a floor could not decide whether those kernels are called. All it could do is cost the
    # archive every machine below the floor.
    cpu_floor='x86-64 baseline (runtime CPU detection for the SSE/AVX primitives)'
    ;;
  linux-aarch64)
    openssl_target=linux-aarch64
    cpu_floor='armv8-a (NEON is mandatory in ARMv8-A)'
    ;;
  windows-x86_64-msvc)
    openssl_target=VC-WIN64A
    windows=1
    # Same reasoning as linux-x86_64: the primitives pick a kernel from cpuid at run time, so a
    # floor decides nothing the archive needs decided. MSVC's default is the x86-64 baseline.
    cpu_floor='x86-64 baseline (runtime CPU detection for the SSE/AVX primitives)'
    ;;
  *)
    echo "unknown target: $target" >&2
    exit 1
    ;;
esac

# A path as the native tool it is handed to will read it. Under MSYS2 that is `C:/…` — forward
# slashes, which cl, cmake, nmake and perl all accept and which MSYS2's argument conversion leaves
# alone — and everywhere else it is the path itself.
np() { if [ -n "$windows" ]; then cygpath -m "$1"; else printf '%s\n' "$1"; fi; }

if [ -n "$windows" ]; then
  case "$(uname -s)" in
    MINGW* | MSYS*) ;;
    *)
      echo "$target is built from MSYS2 bash on a Windows machine, not from $(uname -s)" >&2
      exit 1
      ;;
  esac
  # The developer shell, recognised by the variables it sets rather than by which shell this is.
  # `LIB` is what `link` and `nmake` search, and it is also where the measurement below finds the
  # SDK's import libraries; `INCLUDE` is where cl finds <windows.h>.
  if [ -z "${LIB:-}" ] || [ -z "${INCLUDE:-}" ]; then
    echo "LIB and INCLUDE are not set — run this inside a Visual Studio developer shell" >&2
    echo "  (vcvars64.bat, or ilammy/msvc-dev-cmd on a runner) so cl, link and the SDK are found." >&2
    exit 1
  fi
  for tool in cl nmake nasm ninja cmake llvm-nm llvm-readobj llvm-ar cygpath; do
    command -v "$tool" >/dev/null 2>&1 || {
      echo "$tool is not on PATH" >&2
      exit 1
    }
  done
  # `link` is MSVC's linker *and* coreutils' hard-link tool, and MSYS2 puts /usr/bin first — so
  # the linker is taken from beside cl rather than from PATH.
  msvc_link="$(dirname "$(command -v cl)")/link.exe"
  [ -x "$msvc_link" ] || {
    echo "no link.exe beside $(command -v cl)" >&2
    exit 1
  }
  # OpenSSL's Configure on VC-WIN64A needs a *native* perl: MSYS2's is a Cygwin build whose
  # paths and `$^O` the generated nmake makefile does not understand. Strawberry Perl, or
  # whatever OPENSSL_PERL names.
  native_perl=''
  for candidate in "${OPENSSL_PERL:-}" /c/Strawberry/perl/bin/perl.exe perl; do
    [ -n "$candidate" ] || continue
    command -v "$candidate" >/dev/null 2>&1 || continue
    if [ "$("$candidate" -e 'print $^O' 2>/dev/null)" = MSWin32 ]; then
      native_perl="$candidate"
      break
    fi
  done
  [ -n "$native_perl" ] || {
    echo "no native (MSWin32) perl found for OpenSSL's Configure — install Strawberry Perl or" >&2
    echo "  set OPENSSL_PERL to one" >&2
    exit 1
  }
  # Where a Windows machine keeps OpenSSLs this build must not find: the Win64 installer's,
  # Strawberry's own C toolchain (it ships one), MSYS2's, vcpkg's.
  ignore_prefixes='C:/Program Files/OpenSSL;C:/Program Files/OpenSSL-Win64;C:/Strawberry/c;C:/msys64/usr;C:/msys64/mingw64;C:/msys64/clang64;C:/vcpkg'
  cl_banner="$(cl 2>&1 | head -1 | tr -d '\r' || true)"
  echo ">> toolchain: $cl_banner; nasm $(nasm -v | awk '{print $3}'); ninja $(ninja --version); perl $("$native_perl" -e 'print $^V')"
fi

# The archive container. GNU ar's `D` zeroes the member mtimes and uids that otherwise make two
# builds of identical objects differ; Apple's ar has no equivalent, so macOS is not
# byte-reproducible and the CI job that asserts reproducibility builds linux-x86_64 only. Stated
# rather than papered over. FreeRDP compiles no `__DATE__` or `__TIME__` anywhere (checked), so
# the container is the only source of nondeterminism there is to remove.
#
# Windows is the other one: lib.exe takes neither flag, and MSVC objects carry PDB references and
# timestamps of their own. Like macOS, not asserted reproducible.
cmake_ar_flags=()
openssl_ar_flags=()
if [ "$target" != "macos-arm64" ] && [ -z "$windows" ]; then
  # `CMAKE_C_ARCHIVE_APPEND` is deliberately *not* set alongside these two. cmake only generates
  # an append rule when it needs one — a single-shot `ar qc` covers every archive here — so
  # setting it makes it a variable the project never reads, and the configure-time
  # unused-variable assertion below (the one that caught FREERDP_EXTERNAL_SSL_PATH) would fail on
  # it. That assertion is worth more than covering a rule cmake does not emit, and the case it
  # would cover is not left to trust either: the CI reproducibility job builds linux-x86_64 twice
  # and requires the two libraries to hash the same, which is where an append rule sneaking in
  # would show up.
  cmake_ar_flags=(
    "-DCMAKE_C_ARCHIVE_CREATE=<CMAKE_AR> qcD <TARGET> <LINK_FLAGS> <OBJECTS>"
    "-DCMAKE_C_ARCHIVE_FINISH=<CMAKE_RANLIB> -D <TARGET>"
  )
  openssl_ar_flags=(ARFLAGS=qcD)
fi

rm -rf "$out" "$work"
mkdir -p "$work"

# ---------------------------------------------------------------- OpenSSL

ensure_openssl
ssl_src="$here/build/openssl-${OPENSSL_VERSION}"

# `no-shared no-dso no-module` is the load-bearing trio. OpenSSL 3's provider architecture
# otherwise `dlopen`s a `.so` at first use, and a fully static binary that dlopens a provider it
# did not ship is precisely the `debian:trixie-slim` failure this repository exists to prevent.
# With `no-module` the providers are compiled in.
#
# **`no-legacy` is deliberately absent, and that was measured rather than reasoned.** It reads as
# an obvious cut — legacy is where the deprecated algorithms live — and the first build here used
# it. FreeRDP then said, at run time:
#
#   [WARN][com.winpr.utils.ssl] OpenSSL LEGACY provider failed to load, no md4 support available!
#   [WARN] [SSL] {Digest} * md4: NTLM support not available
#   [WARN] [SSL] {Cipher} * rc4: RDP licensing and RDP security will not work
#
# `winpr/libwinpr/utils/ssl.c` loads the legacy provider with the comment "The legacy provider is
# needed for MD4". MD4 is NTLM, NTLM is CredSSP, and CredSSP is every Windows target there is.
# The verification below asserts MD4 and RC4 are in the finished libcrypto for that reason.
openssl_args=(
  no-shared no-dso no-module no-engine no-tests no-apps no-docs
  --prefix="$(np "$ssl_prefix")" --libdir=lib
)
# `-fPIC` for the toolchains that need telling; cl has no such flag and warns on it. On Windows
# `no-shared` is also what makes OpenSSL compile its static libraries `/MT /Zl`
# (Configurations/10-main.conf): `/Zl` omits the default-library directive from every object, so
# the archives name *no* CRT, and their `malloc`-style references resolve against whichever CRT the
# final link brings — Rust's `msvcrt.lib`, the dynamic one. That is OpenSSL's stated design for
# its static libraries, and it is asserted on the archives below rather than taken from here.
[ -z "$windows" ] && openssl_args+=(-fPIC)
[ "$target" = "macos-arm64" ] && openssl_args+=("-mmacosx-version-min=$MACOS_DEPLOYMENT_TARGET")

# What OpenSSL installs, and what the archives are called once collected. The same off Windows;
# there `libssl.lib` becomes `ssl.lib`, because rustc resolves `static=ssl` on MSVC as `ssl.lib`
# and never as `libssl.lib` — so the MANIFEST's link_order, and build.rs, read the same on every
# target.
if [ -n "$windows" ]; then
  ssl_built=(libssl.lib libcrypto.lib)
  ssl_archives=(ssl.lib crypto.lib)
else
  ssl_built=(libssl.a libcrypto.a)
  ssl_archives=(libssl.a libcrypto.a)
fi

# **The one thing in this build that was not reproducible**, and it was CI that said so rather
# than anybody predicting it: `util/mkbuildinf.pl` writes `#define DATE "built on: <now>"` into
# `crypto/buildinf.h`, which is compiled into `cversion.o` and therefore into `libcrypto.a` and
# nothing else. Two builds an hour apart differed in exactly that one archive while FreeRDP's
# three and `libssl.a` matched to the byte.
#
# The generator reads `SOURCE_DATE_EPOCH` and says in a comment that it honours it "even if it's
# zero or the empty string", so zero is a value it was designed to take rather than one that
# happens to work. Zero rather than a date: the string is a placeholder either way, and an epoch
# nobody chose cannot go stale or start an argument about which date it should have been.
export SOURCE_DATE_EPOCH=0

echo ">> configuring OpenSSL ${OPENSSL_VERSION} ($openssl_target) for $target"
mkdir -p "$work/openssl-build"
(
  cd "$work/openssl-build"
  # Out-of-tree, so two targets built on one machine cannot contaminate each other's object files
  # — OpenSSL's in-tree build leaves them in the source directory.
  if [ -n "$windows" ]; then
    MSYS2_ARG_CONV_EXCL='*' "$native_perl" "$(np "$ssl_src/Configure")" "$openssl_target" "${openssl_args[@]}"
  else
    "$ssl_src/Configure" "$openssl_target" "${openssl_args[@]}" "${openssl_ar_flags[@]+"${openssl_ar_flags[@]}"}"
  fi
)
# JOBS caps the parallelism for a machine with less memory than cores; nmake has none to cap.
jobs="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"
echo ">> building OpenSSL"
# `install_dev` rather than `install`: the headers, the two archives and nothing else. Plain
# `install` also writes man pages, `misc/` scripts and a `certs/` tree, none of which belongs in
# a relocatable prefix that exists to be linked against.
if [ -n "$windows" ]; then
  # `-nologo` rather than `/nologo`, so MSYS2 does not read the option as a path to convert.
  (cd "$work/openssl-build" && nmake -nologo build_libs >/dev/null)
  (cd "$work/openssl-build" && nmake -nologo install_dev >/dev/null)
else
  make -C "$work/openssl-build" -j"$jobs" build_libs >/dev/null
  make -C "$work/openssl-build" install_dev >/dev/null
fi

for archive in "${ssl_built[@]}"; do
  [ -f "$ssl_prefix/lib/$archive" ] || {
    echo "OpenSSL did not install $archive into $ssl_prefix/lib" >&2
    exit 1
  }
done
# And that the epoch above actually reached the generated header, rather than being exported into
# a build that ignored it. Asserted here rather than left to the reproducibility job in CI: that
# job runs on one target and builds twice, so it costs ten minutes to tell us what one grep can,
# and it does not run at all on the two targets it does not cover.
epoch_date="built on: $(LC_ALL=C TZ=UTC perl -e 'print scalar gmtime(0)') UTC"
if ! LC_ALL=C grep -aqF "$epoch_date" "$ssl_prefix/lib/${ssl_built[1]}"; then
  echo "${ssl_built[1]} does not carry the fixed build date, so this build is not reproducible:" >&2
  LC_ALL=C grep -aoE 'built on: [^"]*' "$ssl_prefix/lib/${ssl_built[1]}" | head -1 >&2
  exit 1
fi
echo "   $ssl_prefix/lib/{${ssl_built[0]},${ssl_built[1]}}"

# ---------------------------------------------------------------- FreeRDP

ensure_freerdp
rdp_src="$here/build/freerdp-${FREERDP_VERSION}"

# Every entry here is either a citation or a measurement. The traps, in the order they bite:
#
#   WITH_FFMPEG / WITH_SWSCALE default **ON** (cmake/ConfigOptions.cmake) and become
#     `find_package(… REQUIRED)`. Off, or the configure fails on a machine without them and
#     succeeds — with a runtime dependency — on one that has them.
#   WITH_SMARTCARD_EMULATE defaults ON and is the only thing making zlib mandatory. Turning it
#     off is a *correctness* requirement rather than a size cut: remotex already links libz
#     through flate2←png, and a second zlib in one binary is a duplicate-symbol hazard.
#   WITH_UNICODE_BUILTIN=ON, or `find_package(ICU REQUIRED)` — which is C++, so it would also
#     drag libstdc++ in and make the measured cxx_runtime non-none.
#   WITH_KRB5 and WITH_FUSE default ON on Linux and become REQUIRED find_packages.
#   WITH_PKCS11 defaults ON off-Windows.
#   CHANNEL_DISP and CHANNEL_RDPGFX are DYNAMIC channels, so `define_channel_options` wraps them
#     in `cmake_dependent_option(… "CHANNEL_DRDYNVC" OFF)`. Turning drdynvc off silently deletes
#     both rather than failing.
#   WITH_CLIENT_COMMON=ON with WITH_CLIENT=OFF is a valid pair — client/CMakeLists.txt builds
#     client/common on WITH_CLIENT_COMMON alone.
#   WITH_OPAQUE_SETTINGS=ON keeps `settings_types_private.h` out of settings.h, so bindgen never
#     sees the 600-field `rdp_settings` struct. The largest source of layout risk, removed
#     rather than checked.
#
# **Why disp, rdpgfx, rdpsnd, rdpdr and rdpei are ON in an archive whose first consumer called none
# of them.** `channels/client/CMakeLists.txt` generates `tables.c` at *configure* time from the
# enabled set, and `client/common/CMakeLists.txt` links those OBJECT libraries into
# libfreerdp-client3. The channel list is therefore baked into the archive and a consumer cannot
# enable one later — so resize (disp) and audio (rdpsnd) would each need a new archive and a new
# release. Static linking is per-object: a binary that never calls them pulls in nothing.
cmake_args=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_INSTALL_PREFIX="$(np "$work/prefix")"
  -DBUILD_SHARED_LIBS=OFF
  -DBUILD_TESTING=OFF
  -DCMAKE_POSITION_INDEPENDENT_CODE=ON
  # Rust links these into a position-independent executable, so every object must be PIC.

  # **Link-time optimisation off, and this is the single most important line in the list.**
  # `cmake/CommonConfigOptions.cmake` turns `CMAKE_INTERPROCEDURAL_OPTIMIZATION` on wherever the
  # compiler supports it, which is everywhere this repository builds. Measured consequence: every
  # member of the resulting archives is an **LLVM bitcode file**, not an object file —
  # `otool -l libfreerdp-client3.a` answers "is an LLVM bit-code file" for all 60 of them, and
  # the deployment-target check below reads back nothing at all because a bitcode member has no
  # load commands to read.
  #
  # An archive of bitcode is a *promise about the consumer's compiler*: it can only be linked by
  # a toolchain whose LTO plugin understands the bitcode this one emitted, and LLVM's bitcode
  # compatibility guarantee runs forwards, not sideways. That is precisely the dependency this
  # repository exists to remove — it would build here, link here, pass CI here, and fail on a
  # machine with a different Xcode or a different binutils. The verification below asserts real
  # object code rather than trusting this line.
  -DCMAKE_INTERPROCEDURAL_OPTIMIZATION=OFF

  # FreeRDP asks git what version it is, and `get_git_head_revision` walks *up* from the source
  # directory until it finds a `.git` — which, for a tree unpacked under this repository, is
  # **this repository's**. Measured: with the enclosing repo on an unborn branch the configure
  # fails outright, and with commits it would succeed and stamp a stranger's sha into
  # `freerdp_get_version_string()`. Neither is a version of FreeRDP. The tarball already ships
  # `.source_version` with the real commit, and `RAW_VERSION_STRING` is hard-coded per release,
  # so turning both git paths off makes the build independent of where it was unpacked —
  # a reproducibility property rather than a workaround.
  -DUSE_VERSION_FROM_GIT_TAG=OFF -DUSE_GIT_FOR_REVISION=OFF

  -DFREERDP_UNIFIED_BUILD=ON
  -DWITH_CLIENT_COMMON=ON -DWITH_CLIENT=OFF -DWITH_SERVER=OFF -DWITH_SAMPLE=OFF
  -DWITH_CLIENT_SDL=OFF -DWITH_CLIENT_MAC=OFF -DWITH_SHADOW=OFF -DWITH_PROXY=OFF
  -DWITH_PLATFORM_SERVER=OFF -DWITH_WINPR_TOOLS=OFF -DWITH_RDTK=OFF -DWITH_MANPAGES=OFF
  -DWITH_X11=OFF -DWITH_WAYLAND=OFF -DWITH_WEBVIEW=OFF

  -DWITH_CHANNELS=ON -DWITH_CLIENT_CHANNELS=ON -DWITH_SIMD=ON
  -DWITH_UNICODE_BUILTIN=ON -DWITH_OPAQUE_SETTINGS=ON -DWITH_VERBOSE_WINPR_ASSERT=OFF
  -DWITH_JSON_DISABLED=ON -DWITH_AAD=OFF -DWITH_KRB5=OFF -DWITH_FUSE=OFF -DWITH_PKCS11=OFF

  -DWITH_FFMPEG=OFF -DWITH_SWSCALE=OFF -DWITH_CAIRO=OFF -DWITH_OPENH264=OFF -DWITH_DSP_FFMPEG=OFF
  -DWITH_ALSA=OFF -DWITH_PULSE=OFF -DWITH_OSS=OFF -DWITH_MACAUDIO=OFF
  -DWITH_PCSC=OFF -DWITH_SMARTCARD_EMULATE=OFF -DWITH_CUPS=OFF
  # `WITH_OPUS` defaults ON, and it is not a size question. A runner with libopus-dev installed
  # would fold a second Opus into an archive whose first consumer already links its own static
  # one — the same duplicate-symbol hazard as zlib, and both are asserted against below.
  -DWITH_OPUS=OFF

  -DOPENSSL_ROOT_DIR="$(np "$ssl_prefix")"
  -DOPENSSL_USE_STATIC_LIBS=ON
  -DCMAKE_IGNORE_PREFIX_PATH="$ignore_prefixes"
  -DWITH_MBEDTLS=OFF

  -DCHANNEL_DRDYNVC=ON -DCHANNEL_DRDYNVC_CLIENT=ON
  -DCHANNEL_CLIPRDR=ON -DCHANNEL_CLIPRDR_CLIENT=ON
  -DCHANNEL_DISP=ON -DCHANNEL_DISP_CLIENT=ON
  -DCHANNEL_RDPGFX=ON -DCHANNEL_RDPGFX_CLIENT=ON
  -DCHANNEL_RDPSND=ON -DCHANNEL_RDPSND_CLIENT=ON
  -DCHANNEL_RDPDR=ON -DCHANNEL_RDPDR_CLIENT=ON
  # Touch. `rdpei` is MS-RDPEI, the `Microsoft::Windows::RDS::Input` dynamic channel a Windows
  # host injects real touch contacts from — the one mstsc on a tablet uses, so every shell
  # gesture (edge swipes, pinch, multi-finger) is the host's own rather than an emulation. Like
  # `disp` it is a DVC under `cmake_dependent_option(… CHANNEL_DRDYNVC)`, and like every channel
  # here it has to be in the archive at configure time; the consumer turns it on per session
  # through `FreeRDP_MultiTouchInput`, which `client/common/cmdline.c` maps to the addin.
  -DCHANNEL_RDPEI=ON -DCHANNEL_RDPEI_CLIENT=ON
)

# Every other channel, off by name. A loop rather than a hand-written list, so a channel FreeRDP
# adds in a later release is off by *default* and turned on deliberately, never by inheriting
# somebody's idea of a sensible default.
for channel in ainput audin drive echo encomsp geometry gfxredir location parallel printer rail \
  rdp2tcp rdpear rdpecam rdpemsc rdpewa remdesk serial smartcard sshagent telemetry tsmf \
  urbdrc video; do
  upper="$(tr '[:lower:]' '[:upper:]' <<<"$channel")"
  cmake_args+=("-DCHANNEL_${upper}=OFF")
done

[ "$target" = "macos-arm64" ] && cmake_args+=("-DCMAKE_OSX_DEPLOYMENT_TARGET=$MACOS_DEPLOYMENT_TARGET")

if [ -n "$windows" ]; then
  # Everything above stays on the list, the Linux-only entries included. `CMakeLists.txt` reads
  # every `WITH_*` variable into `buildflags.h` (its FREERDP_BUILD_CONFIG loop), so none of them
  # is ever "unused" on any platform, the assertion below could not tell a Windows-ignored one
  # from a read one, and an identical array keeps the MANIFESTs comparable. What Windows adds:
  cmake_args+=(
    # The dynamic CRT, `/MD` — what Rust's `-msvc` target links, and what libopus-prebuilt and
    # libvpx-prebuilt are built against. Asserted on the archives' directives below.
    -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL
    # Defaults ON on WIN32: a winmm `rdpsnd`/`audin` backend beside the consumer's own, and
    # `winmm.lib` on every consumer's link line for it.
    -DWITH_WINMM=OFF
    # Defaults ON on WIN32: NTLM and CredSSP through secur32 loaded at run time, instead of the
    # WinPR path over the OpenSSL built above — the one the MD4/RC4 assertions and every other
    # target cover.
    -DWITH_NATIVE_SSPI=OFF
    -DWITH_WIN8=OFF -DWITH_MEDIA_FOUNDATION=OFF
    # `_WIN32_WINNT=0x0601`, FreeRDP's own default, spelled out because gen-bindings.sh compiles
    # the same headers with the same value and the two must not drift apart.
    -DCMAKE_WINDOWS_VERSION=WIN7
  )
fi

generator=(-G "Unix Makefiles")
command -v ninja >/dev/null 2>&1 && generator=(-G Ninja)
if [ -n "$windows" ]; then
  # Ninja, required rather than preferred. cmake's default on Windows is the Visual Studio
  # generator, which ignores CMAKE_BUILD_TYPE and builds Debug — an `/MDd` archive nobody can link
  # into a release binary, and one nothing above would notice. Ninja is single-configuration and
  # takes the compiler from the developer shell; `CC=cl` says which, in case the MSYS2 PATH
  # carries a gcc.
  generator=(-G Ninja)
  export CC=cl
fi

echo ">> configuring FreeRDP ${FREERDP_VERSION} for $target"
configure_log="$work/cmake-configure.log"
cmake -S "$(np "$rdp_src")" -B "$(np "$work/cmake")" "${generator[@]}" \
  "${cmake_args[@]}" "${cmake_ar_flags[@]+"${cmake_ar_flags[@]}"}" 2>&1 | tee "$configure_log"

# Two assertions on the configure output, and the first one is the most valuable line in this
# file. cmake does not fail on a `-D` it never read — it prints a warning at the very end and
# carries on. That is how `FREERDP_EXTERNAL_SSL_PATH` (which sits inside an `if(WIN32)` block)
# was silently ignored here while the build linked Homebrew's OpenSSL and passed every test.
if grep -q 'Manually-specified variables were not used by the project' "$configure_log"; then
  echo "cmake ignored one or more of the options above — the build is not what this file says:" >&2
  sed -n '/Manually-specified variables were not used/,$p' "$configure_log" >&2
  exit 1
fi
# And that it found *our* OpenSSL. The check above cannot catch this one: OPENSSL_ROOT_DIR is
# read, so it is never "unused" — it is simply outranked by anything find_package likes better.
grep -qE "Found OpenSSL: .*$(np "$ssl_prefix").* \(found version \"${OPENSSL_VERSION}\"\)" "$configure_log" || {
  echo "cmake did not find the OpenSSL ${OPENSSL_VERSION} built above:" >&2
  grep -i 'OpenSSL' "$configure_log" >&2 || true
  exit 1
}
echo "   OpenSSL ${OPENSSL_VERSION} from $ssl_prefix, and no ignored options"

echo ">> building FreeRDP"
cmake --build "$(np "$work/cmake")" --parallel "$jobs" >/dev/null
cmake --install "$(np "$work/cmake")" >/dev/null

# ---------------------------------------------------------------- collect

# Named rather than globbed, in link order. These four are the whole public surface: everything
# else FreeRDP installs under lib/ is either a pkg-config file naming this machine's paths, a
# cmake package config naming the same, or `lib/freerdp3/` — the OBJECT libraries' install
# artifacts, which are duplicates of objects already inside libfreerdp-client3.a.
#
# The order is a strict DAG with no back-edges, which matters because rustc has no
# `--start-group`: the client archive calls into the core, the core calls into WinPR, and all
# three call into OpenSSL.
archives=(libfreerdp-client3.a libfreerdp3.a libwinpr3.a)
# cmake's static-library prefix on Windows is empty, and rustc's `static=winpr3` there means
# `winpr3.lib` — so FreeRDP's three keep the names cmake gave them, OpenSSL's two lose their
# `lib` (see `ssl_archives`), and the link order is one string on every target.
[ -n "$windows" ] && archives=(freerdp-client3.lib freerdp3.lib winpr3.lib)
link_order='-lfreerdp-client3 -lfreerdp3 -lwinpr3 -lssl -lcrypto'

mkdir -p "$out/lib"
for archive in "${archives[@]}"; do
  cp "$work/prefix/lib/$archive" "$out/lib/$archive"
done
for i in 0 1; do
  cp "$ssl_prefix/lib/${ssl_built[$i]}" "$out/lib/${ssl_archives[$i]}"
done

# The whole installed header tree, not a hand-picked list — and it has to be the *installed* one
# rather than the tarball's. `freerdp/settings_keys.h`, where every `FreeRDP_Xxx` constant lives
# and therefore the entire configuration API, is **generated at configure time** and does not
# exist in the source tarball at all. Same for freerdp/config.h, version.h, build-config.h and
# winpr/config.h.
cp -R "$work/prefix/include/freerdp3" "$out/include-freerdp3"
cp -R "$work/prefix/include/winpr3" "$out/include-winpr3"
mkdir -p "$out/include"
mv "$out/include-freerdp3" "$out/include/freerdp3"
mv "$out/include-winpr3" "$out/include/winpr3"

# FreeRDP's licence and OpenSSL's, from the same verified tarballs. Both travel with the archive
# rather than being left behind in a build tree: whoever links this redistributes both, and each
# licence requires its notice to go along.
cp "$rdp_src/LICENSE" "$out/LICENSE.FreeRDP"
cp "$ssl_src/LICENSE.txt" "$out/LICENSE.OpenSSL"

# ---------------------------------------------------------------- verify

echo ">> verifying the archives hold object code, not LLVM bitcode"
# See the `CMAKE_INTERPROCEDURAL_OPTIMIZATION=OFF` note above for why this matters. It is checked
# rather than assumed because the failure is invisible from here: bitcode archives have symbol
# tables, `nm` reads them, the entry-point checks below pass, and the probe links — with the same
# compiler that produced them. It is a consumer on a different toolchain who finds out.
case "$target" in
  macos-arm64)
    for archive in "${archives[@]}"; do
      if otool -l "$out/lib/$archive" 2>&1 | grep -q 'is an LLVM bit-code file'; then
        echo "$archive is an archive of LLVM bitcode, not object files — LTO is still on" >&2
        exit 1
      fi
    done
    ;;
  linux-*)
    # One member is enough: cmake applies the setting per target, so an archive is all bitcode or
    # none of it. `readelf` rather than `file`, because binutils is present wherever a compiler
    # that produced these is and `file` is a separate package.
    probe_member="$(mktemp -d)"
    for archive in "${archives[@]}"; do
      # The whole listing, then its first line. Not `ar t … | head -1`: `head` closes the pipe on
      # the line it wanted, and under `pipefail` an `ar` that noticed gets the build blamed for it.
      listing="$(ar t "$out/lib/$archive")"
      member="${listing%%$'\n'*}"
      (cd "$probe_member" && ar x "$out/lib/$archive" "$member")
      readelf -h "$probe_member/$member" >/dev/null 2>&1 || {
        echo "$archive's members are not ELF objects — LTO is probably still on" >&2
        rm -rf "$probe_member"
        exit 1
      }
      rm -f "${probe_member:?}/$member"
    done
    rm -rf "$probe_member"
    ;;
  windows-*)
    # COFF has no bitcode to worry about unless `/GL` is on — which IPO OFF prevents — but the
    # same question is asked the same way: the first member of each archive must be an x86-64
    # COFF object. `llvm-ar p` rather than `x`, because lib.exe names members by path and an
    # extraction would want the directories.
    probe_member="$(mktemp -d)"
    for archive in "${archives[@]}"; do
      listing="$(llvm-ar t "$out/lib/$archive")"
      member="${listing%%$'\n'*}"
      llvm-ar p "$out/lib/$archive" "$member" > "$probe_member/member.obj"
      llvm-readobj --file-headers "$probe_member/member.obj" 2>/dev/null | grep -q 'Format: COFF-x86-64' || {
        echo "$archive's first member ($member) is not an x86-64 COFF object" >&2
        rm -rf "$probe_member"
        exit 1
      }
    done
    rm -rf "$probe_member"
    ;;
esac
echo "   real object code"

if [ -n "$windows" ]; then
  echo ">> verifying which CRT the archives name"
  # `/DEFAULTLIB` directives, read off the objects — the one place the CRT choice is recorded. The
  # three FreeRDP archives must name MSVCRT (the dynamic CRT, `/MD`) and no static or debug one;
  # OpenSSL's two must name none at all, which is what `/Zl` means and what lets a `/MT`-configured
  # OpenSSL link into an `/MD` binary. A LIBCMT or MSVCRTD here is a duplicate-CRT link error in
  # every consumer, reported against a symbol nobody would connect to this file.
  for archive in "${archives[@]}"; do
    directives="$(llvm-readobj --coff-directives "$out/lib/$archive")"
    grep -qiE 'DEFAULTLIB:"?MSVCRT"?( |$)' <<<"$directives" || {
      echo "$archive names no MSVCRT default library — it was not compiled with /MD" >&2
      exit 1
    }
    if grep -qiE 'DEFAULTLIB:"?(LIBCMT|LIBCMTD|MSVCRTD)"?( |$)' <<<"$directives"; then
      echo "$archive names a static or debug CRT:" >&2
      grep -ioE 'DEFAULTLIB:"?(LIBCMT|LIBCMTD|MSVCRTD)"?' <<<"$directives" | sort -u >&2
      exit 1
    fi
  done
  for archive in "${ssl_archives[@]}"; do
    if llvm-readobj --coff-directives "$out/lib/$archive" | grep -qi 'DEFAULTLIB'; then
      echo "$archive names a default library, so OpenSSL was not built /Zl and its CRT choice" >&2
      echo "  would fight the consumer's" >&2
      exit 1
    fi
  done
  echo "   FreeRDP: MSVCRT (dynamic, /MD); OpenSSL: none (/Zl)"
fi

echo ">> verifying the entry points are in the archives"
# The functions the wrapper crate actually calls, and the channel entry points that say the
# channel set really is what was configured. An archive that landed under the right name with the
# wrong contents fails here rather than at the link step of every consumer.
#
# No `2>/dev/null || true` on the nm: an nm that cannot read an archive produces an empty symbol
# list, and an empty symbol list makes every check below report a *missing* entry point — a
# measurement failure wearing the costume of a build failure.
#
# `nm` on ELF and Mach-O, `llvm-nm` on COFF — minus the `member.obj:` header lines and blank
# separators it prints per archive member, which would otherwise be read as symbols. x64 COFF
# names carry no underscore prefix, so `[ _]name$` matches the same on every target.
list_symbols() {
  if [ -n "$windows" ]; then
    llvm-nm "$1" "$2" | grep -v -e ':$' -e '^$'
  else
    nm "$1" "$2"
  fi
}
symbols=''
for archive in "${archives[@]}"; do
  part="$(list_symbols --defined-only "$out/lib/$archive")" || {
    echo "nm could not read $out/lib/$archive — nothing below was measured" >&2
    exit 1
  }
  symbols+="$part"$'\n'
done

# `[ _]` because Mach-O prefixes every C symbol with an underscore and ELF does not. A here-string
# rather than `printf … | grep -q`, because under `set -o pipefail` grep -q exits on the first
# match, the writer takes SIGPIPE, and the pipeline reports 141 — so a *found* symbol would read
# as a missing one.
require_symbol() {
  grep -qE "[ _]$1$" <<<"$symbols" || {
    echo "$1 is not defined in the archives — this is not the FreeRDP this file configures" >&2
    exit 1
  }
}

entry_points='freerdp_client_context_new freerdp_client_context_free
              freerdp_connect freerdp_disconnect freerdp_abort_connect_context
              freerdp_get_event_handles freerdp_check_event_handles
              freerdp_settings_set_bool freerdp_settings_set_uint32
              freerdp_settings_set_string freerdp_settings_get_uint32
              freerdp_get_last_error freerdp_get_last_error_string
              freerdp_input_send_mouse_event freerdp_input_send_extended_mouse_event
              freerdp_input_send_keyboard_event freerdp_input_send_unicode_keyboard_event
              gdi_init gdi_free gdi_graphics_pipeline_init graphics_register_pointer
              freerdp_client_load_addins PubSub_Subscribe freerdp_client_handle_touch'
# `PubSub_Subscribe` rather than `PubSub_SubscribeChannelConnected`, which is what the wrapper
# reads like in C: WinPR generates the per-event subscribers as `static inline` functions from a
# macro, so they exist in no archive and bindgen emits none of them. The wrapper calls the
# variadic base function with the event name as a string, which is what the inline would do.
for symbol in $entry_points; do require_symbol "$symbol"; done
echo "   $(printf '%s\n' "$entry_points" | wc -w | tr -d ' ') entry points defined"

echo ">> verifying the channels are compiled in"
# Per channel, the symbol its own registration goes through — a static virtual-channel entry for
# the SVCs, a DVC plugin entry for the dynamic ones. This is how "the channel is really
# in there" is known rather than believed, and it is what would silently go missing if a
# `CHANNEL_*` option were renamed upstream and the `-D` above became a no-op. (It could not
# become a *silent* no-op — the unused-variable check above sees to that — but a channel can also
# be dropped by its own `cmake_dependent_option`, which leaves no warning at all.)
channels=''
for entry in cliprdr_VirtualChannelEntryEx:cliprdr rdpdr_VirtualChannelEntryEx:rdpdr \
  rdpsnd_VirtualChannelEntryEx:rdpsnd drdynvc_VirtualChannelEntryEx:drdynvc \
  disp_DVCPluginEntry:disp rdpgfx_DVCPluginEntry:rdpgfx rdpei_DVCPluginEntry:rdpei; do
  require_symbol "${entry%%:*}"
  channels+="${entry##*:} "
done
channels="${channels% }"
echo "   $channels"

echo ">> verifying no second copy of a library the consumer already links"
# zlib and Opus, by name, because both are hazards rather than preferences: the project this was
# built for already links its own static zlib (through flate2←png) and its own static libopus,
# and a second copy of either inside these archives is a duplicate-symbol failure at *its* link
# step, reported against a symbol nobody here would recognise.
#
# Turning `WITH_SMARTCARD_EMULATE` and `WITH_OPUS` off is what prevents it; this is what says so.
# Both directions matter — a *defined* copy collides, and an *undefined* reference means the
# consumer's own copy silently wins the link and serves FreeRDP too.
for symbol in inflateInit_ deflateInit_ compress2 opus_encode opus_decoder_create; do
  if grep -qE "[ _]$symbol$" <<<"$symbols"; then
    echo "$symbol is *defined* in the archives — something bundled a library the consumer" >&2
    echo "  already links. Check WITH_SMARTCARD_EMULATE and WITH_OPUS." >&2
    exit 1
  fi
done

echo ">> verifying OpenSSL kept the algorithms CredSSP needs"
# MD4 is NTLM and RC4 is RDP licensing, both reached through the legacy provider — see the
# `no-legacy` note above. An OpenSSL configured without it still links, still connects to a Linux
# xrdp over TLS, and fails only against a Windows host doing NLA, which is the one case that
# matters most and the one a CI job on a container image does not exercise.
ssl_symbols="$(list_symbols --defined-only "$out/lib/${ssl_archives[1]}")" || {
  echo "nm could not read $out/lib/${ssl_archives[1]} — the provider check did not run" >&2
  exit 1
}
for symbol in ossl_md4_functions ossl_rc4128_functions; do
  grep -qE "[ _]$symbol$" <<<"$ssl_symbols" || {
    echo "$symbol is not in ${ssl_archives[1]} — the legacy provider was configured out, so NTLM" >&2
    echo "  (and therefore CredSSP, and therefore every Windows target) will not work." >&2
    exit 1
  }
done
echo "   md4 and rc4 present"

# ---------------------------------------------------------------- measure

# Which system libraries and frameworks the archives need. Measured from the symbols and then
# **asserted by linking a probe with exactly that set and nothing else** — a list that was
# measured and never tested is a list that build.rs would emit forever after it went wrong.
echo ">> measuring the system dependencies"
undefined=''
defined=''
for archive in "${archives[@]}" "${ssl_archives[@]}"; do
  part="$(list_symbols --undefined-only "$out/lib/$archive")" || {
    echo "nm could not read $out/lib/$archive — the requirements were not measured" >&2
    exit 1
  }
  undefined+="$part"$'\n'
  # OpenSSL's two archives as well as FreeRDP's three, which is why `$symbols` from the section
  # above is not reused: libssl calls into libcrypto for nearly everything, and without libcrypto
  # in the subtrahend every one of those references looks like a system dependency.
  part="$(list_symbols --defined-only "$out/lib/$archive")" || {
    echo "nm could not read $out/lib/$archive — the requirements were not measured" >&2
    exit 1
  }
  defined+="$part"$'\n'
done

# Undefined *minus* defined, which is the set that has to come from outside. Five archives that
# call into each other leave ~10,000 undefined symbols between them, ~97% of which another one of
# the five defines — libssl's references into libcrypto alone are most of it. Subtracting is what
# turns the question "which of these needs a system library" from a guess about symbol naming
# into a list of 302 names a person can read.
external="$(comm -23 \
  <(awk '{print $NF}' <<<"$undefined" | sort -u) \
  <(awk '{print $NF}' <<<"$defined" | sort -u))"
echo "   $(wc -l <<<"$external" | tr -d ' ') symbols must come from outside the archives"

# The greps below are the one place where "found nothing" is an answer rather than a fault, so
# they accept exit 1 and nothing else: exit 2 is grep saying it could not do the search, which is
# indistinguishable from a match-free archive if it is thrown away.
# Match a pattern against the external set. This is the one place where "found nothing" is an
# answer rather than a fault, so it accepts exit 1 and nothing else: exit 2 is grep saying it
# could not do the search, which is indistinguishable from a match-free archive if it is thrown
# away. Matched symbols are recorded, so what is left over can be reported when the probe fails.
matched=''
matches() {
  local status=0 found
  found="$(grep -E "$1" <<<"$external")" || status=$?
  [ "$status" -le 1 ] || {
    echo "grep failed ($status) while measuring '$1' — the requirement is unknown, not absent" >&2
    exit 1
  }
  [ -n "$found" ] || return 1
  matched+="$found"$'\n'
}

system_libs=()
frameworks=()
case "$target" in
  macos-arm64)
    # libm, libdl, libpthread and libdispatch are all part of libSystem on Darwin, which Rust's
    # own std already links — so there is nothing to emit for them and the only question is
    # frameworks.
    matches '^_k?CF[A-Z]' && frameworks+=(CoreFoundation)
    # CoreServices, and rdpdr is what needs it: its drive-hotplug thread watches for volumes with
    # `FSEventStreamCreate`, and that code compiles even with CHANNEL_DRIVE off. Found by the
    # probe below refusing to link, which is exactly what the probe is for.
    matches '^_(LS[A-Z]|FSPath|FSGetCatalogInfo|FSEventStream)' && frameworks+=(CoreServices)
    # Carbon, for Text Input Services — WinPR asks the current keyboard layout for the RDP
    # keyboard type, through `TISCopyCurrentKeyboardLayoutInputSource`.
    matches '^_(k?TIS[A-Z]|Gestalt|GetCurrentProcess)' && frameworks+=(Carbon)
    # Neither of these two is expected. Foundation would mean WinPR's Objective-C unicode path
    # came back, which `WITH_UNICODE_BUILTIN=ON` exists to prevent; Security would mean something
    # started using the Keychain instead of the OpenSSL built above. Both are here so that if
    # they ever do appear, the answer is a framework in the MANIFEST rather than a link error.
    matches '^_(objc_|OBJC_|NSLog)' && frameworks+=(Foundation)
    matches '^_(SecItem|SecKeychain|SecCertificate|SecTrust)' && frameworks+=(Security)
    matches '^_(IOService|IOMasterPort|IOIterator|IORegistry)' && frameworks+=(IOKit)
    ;;
  linux-*)
    matches '^(pow|exp|log|log2|log10|sqrt|floor|ceil|fabs|fmod|round|lround|sin|cos|atan2|ldexp|frexp)$' \
      && system_libs+=(m)
    matches '^(dlopen|dlsym|dlclose|dladdr|dlerror|dlvsym)$' && system_libs+=(dl)
    matches '^pthread_' && system_libs+=(pthread)
    matches '^(shm_open|shm_unlink|timer_create|timer_settime|aio_read)$' && system_libs+=(rt)
    ;;
  windows-*)
    # Import libraries, measured against the SDK's own rather than guessed from names. Every
    # `__imp_Name` the archives reference is looked up in the import libraries a candidate list
    # names — `llvm-nm` lists an import library's `__imp_` symbols like any archive's — and the
    # first library defining it claims it. `kernel32` goes first because several exports appear
    # in more than one library and kernel32's is the copy everything links anyway. The CRT's own
    # (msvcrt, ucrt, vcruntime, oldnames) come last and are claimed but *not* recorded: MSVCRT is
    # what the `/DEFAULTLIB` directive asserted above already brings to every consumer's link.
    # Whatever is left unclaimed is printed and left to the probe link, which is the oracle.
    sdk_lib_dirs=()
    while IFS= read -r dir; do
      [ -n "$dir" ] && sdk_lib_dirs+=("$(cygpath -u "$dir")")
    done < <(tr ';' '\n' <<<"$LIB")
    find_import_lib() {
      local dir
      for dir in "${sdk_lib_dirs[@]}"; do
        [ -f "$dir/$1.lib" ] && {
          printf '%s\n' "$dir/$1.lib"
          return 0
        }
      done
      return 1
    }
    # Both spellings. A call through the import table is `__imp_Name`; a call WinPR makes through
    # its own declaration of an API — the NCrypt and Nt* families, SHGetKnownFolderPath — is a
    # bare `Name`, and an import library defines both. uuid.lib is not an import library at all:
    # it holds the `FOLDERID_*` GUIDs as plain data. Windows API names are capitalised, which is
    # what separates them here from the lowercase CRT (already brought by `/DEFAULTLIB`).
    # `__ImageBase` is defined by the linker itself and belongs to no library.
    unclaimed="$(grep -E '^(__imp_)?[A-Z]' <<<"$external" | grep -vx '__ImageBase' || true)"
    matched+='__ImageBase'$'\n'
    for candidate in kernel32 user32 advapi32 ws2_32 crypt32 secur32 rpcrt4 shlwapi shell32 \
      gdi32 ole32 credui cfgmgr32 dbghelp bcrypt ncrypt ntdll uuid iphlpapi setupapi winmm mpr \
      netapi32 userenv version msvcrt ucrt vcruntime oldnames; do
      [ -n "$unclaimed" ] || break
      lib="$(find_import_lib "$candidate")" || continue
      provided="$(llvm-nm --defined-only "$(np "$lib")" 2>/dev/null | tr -d '\r' | grep -v ':$' | awk 'NF > 1 {print $NF}' | sort -u)"
      [ -n "$provided" ] || continue
      claimed="$(comm -12 <(sort -u <<<"$unclaimed") <(printf '%s\n' "$provided"))"
      [ -n "$claimed" ] || continue
      matched+="$claimed"$'\n'
      unclaimed="$(comm -23 <(sort -u <<<"$unclaimed") <(printf '%s\n' "$claimed"))"
      case "$candidate" in
        msvcrt | ucrt | vcruntime | oldnames) ;;
        *) system_libs+=("$candidate") ;;
      esac
    done
    if [ -n "$unclaimed" ]; then
      echo "   imports no candidate import library claims (the probe link decides):"
      while IFS= read -r line; do echo "     $line"; done <<<"$unclaimed"
    fi
    ;;
esac

cxx_runtime='none'
# MSVC spells its C++ runtime differently: `??2@YAPEAX_K@Z` is operator new, and anything in
# `@std@@` is the standard library.
cxx_pattern='^_?(_Zn[wa]|_Zd[la]|_ZN?St[0-9]|__cxa_(throw|begin_catch|allocate)|__gxx_personality)'
[ -n "$windows" ] && cxx_pattern='^(__imp_)?(\?\?[23]@YA|__CxxFrameHandler|_CxxThrowException|\?[^ ]*@std@@)'
if matches "$cxx_pattern"; then
  echo "   cxx_runtime: required — something in this build pulled in C++" >&2
  echo "     (WITH_UNICODE_BUILTIN=OFF would do it, via ICU; so would a C++ codec.)" >&2
  echo "     Refused rather than recorded: a C++ runtime is a second dependency for every" >&2
  echo "     consumer, and nothing this repository configures should need one." >&2
  exit 1
fi

echo "   system_libs: ${system_libs[*]:-none}"
echo "   frameworks:  ${frameworks[*]:-none}"
echo "   cxx_runtime: $cxx_runtime"

# The probe. Compiled and linked against exactly what was measured, then run.
#
# This is where "the archives are complete" stops being a symbol count. A link resolves every
# transitive reference in the objects the probe pulls in, in the order recorded above, with the
# system library set recorded above — so a missing back-edge in the link order, a system library
# nobody noticed, and an archive built for the wrong ABI all fail here, on the machine that built
# them, rather than in a consumer's cargo build.
echo ">> linking and running a probe against the archives"
cat > "$work/probe.c" <<'PROBE'
#include <stdio.h>
#include <freerdp/freerdp.h>
#include <freerdp/client.h>
#include <freerdp/gdi/gdi.h>
#include <freerdp/settings.h>
#include <freerdp/version.h>

int main(void)
{
	RDP_CLIENT_ENTRY_POINTS entry = { 0 };
	entry.Size = sizeof(entry);
	entry.Version = RDP_CLIENT_INTERFACE_VERSION;
	entry.ContextSize = sizeof(rdpClientContext);

	rdpContext* context = freerdp_client_context_new(&entry);
	if (!context)
	{
		fprintf(stderr, "freerdp_client_context_new returned NULL\n");
		return 1;
	}
	/* An opaque-settings round trip, which is the whole configuration API in one call pair. */
	if (!freerdp_settings_set_bool(context->settings, FreeRDP_RedirectClipboard, TRUE))
	{
		fprintf(stderr, "freerdp_settings_set_bool failed\n");
		return 1;
	}
	if (!freerdp_settings_get_bool(context->settings, FreeRDP_RedirectClipboard))
	{
		fprintf(stderr, "the setting did not round-trip\n");
		return 1;
	}
	printf("%s\n", freerdp_get_version_string());
	freerdp_client_context_free(context);
	return 0;
}
PROBE

probe_flags=(
  -I "$out/include/freerdp3" -I "$out/include/winpr3"
  "$work/probe.c" -o "$work/probe"
  -L "$out/lib" -lfreerdp-client3 -lfreerdp3 -lwinpr3 -lssl -lcrypto
)
for lib in ${system_libs[@]+"${system_libs[@]}"}; do probe_flags+=("-l$lib"); done
for framework in ${frameworks[@]+"${frameworks[@]}"}; do probe_flags+=(-framework "$framework"); done
[ "$target" = "macos-arm64" ] && probe_flags+=("-mmacosx-version-min=$MACOS_DEPLOYMENT_TARGET")

probe_exe="$work/probe"
link_probe() {
  [ -n "$windows" ] || {
    cc "${probe_flags[@]}"
    return
  }
  probe_exe="$work/probe.exe"
  # `cl -c` and then `link` by hand rather than `cl` end to end: the cl driver would add its own
  # default libraries (kernel32, user32, advapi32, …) to the link, and the point of the probe is
  # to link against the measured set and nothing else. `-MD` so the probe names the same CRT the
  # archives do. Conversion off, because every path here is already native.
  local link_args=("$(np "$work/probe.obj")" "-libpath:$(np "$out/lib")" "${archives[@]}" "${ssl_archives[@]}")
  local lib
  for lib in ${system_libs[@]+"${system_libs[@]}"}; do link_args+=("$lib.lib"); done
  # `-DFREERDP_EXPORTS`: freerdp/api.h marks every API `__declspec(dllimport)` on Windows unless
  # that is defined — there is no static-library spelling — and a dllimport call wants the
  # `__imp_` symbol a static archive does not have. WinPR's own header guards the same choice
  # behind `WINPR_DLL`, which a static build leaves undefined. Every C consumer of these archives
  # defines it likewise; the Rust bindings carry no storage class and need nothing.
  MSYS2_ARG_CONV_EXCL='*' cl -nologo -c -MD -O2 -DWIN32_LEAN_AND_MEAN -DFREERDP_EXPORTS \
    -I"$(np "$out/include/freerdp3")" -I"$(np "$out/include/winpr3")" \
    -Fo"$(np "$work/probe.obj")" "$(np "$work/probe.c")" >/dev/null \
    && MSYS2_ARG_CONV_EXCL='*' "$msvc_link" -nologo -out:"$(np "$probe_exe")" "${link_args[@]}"
}

link_probe || {
  echo "the probe did not link against the measured dependency set" >&2
  echo "  system_libs: ${system_libs[*]:-none}" >&2
  echo "  frameworks:  ${frameworks[*]:-none}" >&2
  echo "  Whatever the linker named above is missing from the measurement in this script." >&2
  echo >&2
  # The external symbols no pattern claimed, minus the ones every libc has. Whatever the linker
  # complained about is in this list, and this list is short enough to read — which is the
  # difference between "add a framework" and "work out which framework".
  echo "  external symbols no pattern above accounted for, other than plain libc:" >&2
  comm -23 <(sort -u <<<"$external") <(sort -u <<<"$matched") \
    | grep -vE '^_?_*[a-z]' | sed 's/^/    /' >&2 || true
  exit 1
}

probe_version="$("$probe_exe" | tr -d '\r')" || {
  echo "the probe linked but did not run cleanly" >&2
  exit 1
}
echo "   probe reports: $probe_version"
[ "${probe_version#"$FREERDP_VERSION"}" != "$probe_version" ] || {
  echo "the probe reports '$probe_version', which does not start with $FREERDP_VERSION" >&2
  exit 1
}

# And that the probe carries FreeRDP inside it rather than expecting to find one. Same question
# check-static.sh asks of a consumer's binary, asked here first so a bad archive fails in the job
# that built it.
case "$(uname -s)" in
  Darwin) probe_deps="$(otool -L "$probe_exe" | tail -n +2)" ;;
  MINGW* | MSYS*) probe_deps="$(llvm-readobj --coff-imports "$probe_exe" | sed -n 's/^ *Name: //p')" ;;
  *) probe_deps="$(ldd "$probe_exe" 2>/dev/null || true)" ;;
esac
# `freerdp|winpr` rather than `libfreerdp|libwinpr`: a Windows DLL of either would be
# `freerdp3.dll`, with no `lib`.
if dynamic="$(grep -iE 'libssl|libcrypto|freerdp|winpr' <<<"$probe_deps")" && [ -n "$dynamic" ]; then
  echo "the probe has dynamic dependencies it should have linked statically:" >&2
  printf '  %s\n' "$dynamic" >&2
  exit 1
fi
echo "   no dynamic libssl/libcrypto/libfreerdp/libwinpr"

# The deployment target, read back off the finished archives rather than trusted from the flag. A
# `-mmacosx-version-min` that a build system dropped on the floor produces *working* archives
# wearing a MANIFEST that lies about which machines they link into.
if [ "$target" = "macos-arm64" ]; then
  echo ">> verifying the deployment target"
  for archive in "${archives[@]}" "${ssl_archives[@]}"; do
    minos="$(otool -l "$out/lib/$archive" 2>/dev/null | awk '/minos/ {print $2}' | sort -u)"
    [ "$minos" = "$MACOS_DEPLOYMENT_TARGET" ] || {
      echo "$archive claims minos '$minos', not $MACOS_DEPLOYMENT_TARGET" >&2
      echo "  (more than one value means some objects missed the flag)" >&2
      exit 1
    }
  done
  echo "   minos $MACOS_DEPLOYMENT_TARGET on every member of every archive"
fi

# ---------------------------------------------------------------- manifest

echo ">> checksumming"
{
  echo "freerdp $FREERDP_VERSION"
  echo "openssl $OPENSSL_VERSION"
  echo "target $target"
  echo "sha256(freerdp-source) $FREERDP_SHA256"
  echo "sha256(openssl-source) $OPENSSL_SHA256"
  # Per archive, and the library's own hash rather than the tarball's. A .tar.gz is not
  # reproducible — gzip stamps an mtime — so the wrapper's checksum can only say "these are the
  # bytes that were published". These say something stronger: *this is the same library*,
  # comparable across runs, machines and releases.
  for archive in "${archives[@]}" "${ssl_archives[@]}"; do
    echo "sha256(lib/$archive) $(sha256_of "$out/lib/$archive")"
  done
  echo "libraries ${archives[*]} ${ssl_archives[*]}"
  echo "link_order $link_order"
  echo "system_libs ${system_libs[*]:-none}"
  echo "frameworks ${frameworks[*]:-none}"
  echo "cxx_runtime $cxx_runtime"
  # Which C runtime the archives were compiled against. Informational off Windows; on it, the
  # measured answer to the question every MSVC consumer has to ask.
  if [ -n "$windows" ]; then echo "crt msvcrt (dynamic, /MD; OpenSSL /Zl)"; else echo "crt libc"; fi
  echo "channels $channels"
  echo "cpu_floor $cpu_floor"
  echo "probe $probe_version"
  echo "openssl_args ${openssl_args[*]}"
  echo "cmake_args ${cmake_args[*]}"
} > "$out/MANIFEST"

# `-DCMAKE_INSTALL_PREFIX` and `-DOPENSSL_ROOT_DIR` carry this machine's absolute paths, and the
# MANIFEST is meant to be comparable between machines that built the same thing. Rewritten to a
# placeholder rather than dropped, so the line still shows that they were passed.
sed -i.bak "s#$here#\$REPO#g" "$out/MANIFEST" && rm -f "$out/MANIFEST.bak"
if [ -n "$windows" ]; then
  # The same paths again in their native spelling, which is how cmake was handed them.
  sed -i.bak "s#$(np "$here")#\$REPO#g" "$out/MANIFEST" && rm -f "$out/MANIFEST.bak"
fi

echo ">> wrote $out"
cat "$out/MANIFEST"
