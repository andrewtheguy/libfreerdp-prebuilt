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
# No Windows target. FreeRDP builds fine on MSVC, but no consumer of this repository targets it
# and the OpenSSL half would need its own toolchain setup. Adding one is real work rather than a
# line in the case statement below.
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
  *)
    echo "unknown target: $target" >&2
    exit 1
    ;;
esac

# The archive container. GNU ar's `D` zeroes the member mtimes and uids that otherwise make two
# builds of identical objects differ; Apple's ar has no equivalent, so macOS is not
# byte-reproducible and the CI job that asserts reproducibility builds linux-x86_64 only. Stated
# rather than papered over. FreeRDP compiles no `__DATE__` or `__TIME__` anywhere (checked), so
# the container is the only source of nondeterminism there is to remove.
cmake_ar_flags=()
openssl_ar_flags=()
if [ "$target" != "macos-arm64" ]; then
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
  --prefix="$ssl_prefix" --libdir=lib
  -fPIC
)
[ "$target" = "macos-arm64" ] && openssl_args+=("-mmacosx-version-min=$MACOS_DEPLOYMENT_TARGET")

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
  "$ssl_src/Configure" "$openssl_target" "${openssl_args[@]}" "${openssl_ar_flags[@]+"${openssl_ar_flags[@]}"}"
)
echo ">> building OpenSSL"
make -C "$work/openssl-build" -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" build_libs >/dev/null
# `install_dev` rather than `install`: the headers, the two archives and nothing else. Plain
# `install` also writes man pages, `misc/` scripts and a `certs/` tree, none of which belongs in
# a relocatable prefix that exists to be linked against.
make -C "$work/openssl-build" install_dev >/dev/null

for archive in libssl.a libcrypto.a; do
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
if ! LC_ALL=C grep -aqF "$epoch_date" "$ssl_prefix/lib/libcrypto.a"; then
  echo "libcrypto.a does not carry the fixed build date, so this build is not reproducible:" >&2
  LC_ALL=C grep -aoE 'built on: [^"]*' "$ssl_prefix/lib/libcrypto.a" | head -1 >&2
  exit 1
fi
echo "   $ssl_prefix/lib/{libssl,libcrypto}.a"

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
# **Why disp, rdpgfx, rdpsnd and rdpdr are ON in an archive whose first consumer calls none of
# them.** `channels/client/CMakeLists.txt` generates `tables.c` at *configure* time from the
# enabled set, and `client/common/CMakeLists.txt` links those OBJECT libraries into
# libfreerdp-client3. The channel list is therefore baked into the archive and a consumer cannot
# enable one later — so resize (disp) and audio (rdpsnd) would each need a new archive and a new
# release. Static linking is per-object: a binary that never calls them pulls in nothing.
cmake_args=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_INSTALL_PREFIX="$work/prefix"
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

  -DOPENSSL_ROOT_DIR="$ssl_prefix"
  -DOPENSSL_USE_STATIC_LIBS=ON
  -DCMAKE_IGNORE_PREFIX_PATH="$ignore_prefixes"
  -DWITH_MBEDTLS=OFF

  -DCHANNEL_DRDYNVC=ON -DCHANNEL_DRDYNVC_CLIENT=ON
  -DCHANNEL_CLIPRDR=ON -DCHANNEL_CLIPRDR_CLIENT=ON
  -DCHANNEL_DISP=ON -DCHANNEL_DISP_CLIENT=ON
  -DCHANNEL_RDPGFX=ON -DCHANNEL_RDPGFX_CLIENT=ON
  -DCHANNEL_RDPSND=ON -DCHANNEL_RDPSND_CLIENT=ON
  -DCHANNEL_RDPDR=ON -DCHANNEL_RDPDR_CLIENT=ON
)

# Every other channel, off by name. A loop rather than a hand-written list, so a channel FreeRDP
# adds in a later release is off by *default* and turned on deliberately, never by inheriting
# somebody's idea of a sensible default.
for channel in ainput audin drive echo encomsp geometry gfxredir location parallel printer rail \
  rdp2tcp rdpear rdpecam rdpei rdpemsc rdpewa remdesk serial smartcard sshagent telemetry tsmf \
  urbdrc video; do
  upper="$(tr '[:lower:]' '[:upper:]' <<<"$channel")"
  cmake_args+=("-DCHANNEL_${upper}=OFF")
done

[ "$target" = "macos-arm64" ] && cmake_args+=("-DCMAKE_OSX_DEPLOYMENT_TARGET=$MACOS_DEPLOYMENT_TARGET")

generator=(-G "Unix Makefiles")
command -v ninja >/dev/null 2>&1 && generator=(-G Ninja)

echo ">> configuring FreeRDP ${FREERDP_VERSION} for $target"
configure_log="$work/cmake-configure.log"
cmake -S "$rdp_src" -B "$work/cmake" "${generator[@]}" \
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
grep -qE "Found OpenSSL: .*${ssl_prefix}.* \(found version \"${OPENSSL_VERSION}\"\)" "$configure_log" || {
  echo "cmake did not find the OpenSSL ${OPENSSL_VERSION} built above:" >&2
  grep -i 'OpenSSL' "$configure_log" >&2 || true
  exit 1
}
echo "   OpenSSL ${OPENSSL_VERSION} from $ssl_prefix, and no ignored options"

echo ">> building FreeRDP"
cmake --build "$work/cmake" --parallel "$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" >/dev/null
cmake --install "$work/cmake" >/dev/null

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
ssl_archives=(libssl.a libcrypto.a)
link_order='-lfreerdp-client3 -lfreerdp3 -lwinpr3 -lssl -lcrypto'

mkdir -p "$out/lib"
for archive in "${archives[@]}"; do
  cp "$work/prefix/lib/$archive" "$out/lib/$archive"
done
for archive in "${ssl_archives[@]}"; do
  cp "$ssl_prefix/lib/$archive" "$out/lib/$archive"
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
      member="$(ar t "$out/lib/$archive" | head -1)"
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
esac
echo "   real object code"

echo ">> verifying the entry points are in the archives"
# The functions the wrapper crate actually calls, and the channel entry points that say the
# channel set really is what was configured. An archive that landed under the right name with the
# wrong contents fails here rather than at the link step of every consumer.
#
# No `2>/dev/null || true` on the nm: an nm that cannot read an archive produces an empty symbol
# list, and an empty symbol list makes every check below report a *missing* entry point — a
# measurement failure wearing the costume of a build failure.
symbols=''
for archive in "${archives[@]}"; do
  part="$(nm --defined-only "$out/lib/$archive")" || {
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
              freerdp_client_load_addins PubSub_Subscribe'
# `PubSub_Subscribe` rather than `PubSub_SubscribeChannelConnected`, which is what the wrapper
# reads like in C: WinPR generates the per-event subscribers as `static inline` functions from a
# macro, so they exist in no archive and bindgen emits none of them. The wrapper calls the
# variadic base function with the event name as a string, which is what the inline would do.
for symbol in $entry_points; do require_symbol "$symbol"; done
echo "   $(printf '%s\n' "$entry_points" | wc -w | tr -d ' ') entry points defined"

echo ">> verifying the channels are compiled in"
# Per channel, the symbol its own registration goes through — a static virtual-channel entry for
# the two SVCs, a DVC plugin entry for the three dynamic ones. This is how "the channel is really
# in there" is known rather than believed, and it is what would silently go missing if a
# `CHANNEL_*` option were renamed upstream and the `-D` above became a no-op. (It could not
# become a *silent* no-op — the unused-variable check above sees to that — but a channel can also
# be dropped by its own `cmake_dependent_option`, which leaves no warning at all.)
channels=''
for entry in cliprdr_VirtualChannelEntryEx:cliprdr rdpdr_VirtualChannelEntryEx:rdpdr \
  rdpsnd_VirtualChannelEntryEx:rdpsnd drdynvc_VirtualChannelEntryEx:drdynvc \
  disp_DVCPluginEntry:disp rdpgfx_DVCPluginEntry:rdpgfx; do
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
ssl_symbols="$(nm --defined-only "$out/lib/libcrypto.a")" || {
  echo "nm could not read $out/lib/libcrypto.a — the provider check did not run" >&2
  exit 1
}
for symbol in ossl_md4_functions ossl_rc4128_functions; do
  grep -qE "[ _]$symbol$" <<<"$ssl_symbols" || {
    echo "$symbol is not in libcrypto.a — the legacy provider was configured out, so NTLM" >&2
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
  part="$(nm --undefined-only "$out/lib/$archive")" || {
    echo "nm could not read $out/lib/$archive — the requirements were not measured" >&2
    exit 1
  }
  undefined+="$part"$'\n'
  # OpenSSL's two archives as well as FreeRDP's three, which is why `$symbols` from the section
  # above is not reused: libssl calls into libcrypto for nearly everything, and without libcrypto
  # in the subtrahend every one of those references looks like a system dependency.
  part="$(nm --defined-only "$out/lib/$archive")" || {
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
esac

cxx_runtime='none'
if matches '^_?(_Zn[wa]|_Zd[la]|_ZN?St[0-9]|__cxa_(throw|begin_catch|allocate)|__gxx_personality)'; then
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

cc "${probe_flags[@]}" || {
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

probe_version="$("$work/probe")" || {
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
  Darwin) probe_deps="$(otool -L "$work/probe" | tail -n +2)" ;;
  *) probe_deps="$(ldd "$work/probe" 2>/dev/null || true)" ;;
esac
if dynamic="$(grep -iE 'libssl|libcrypto|libfreerdp|libwinpr' <<<"$probe_deps")" && [ -n "$dynamic" ]; then
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

echo ">> wrote $out"
cat "$out/MANIFEST"
