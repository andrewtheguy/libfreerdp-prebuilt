#!/usr/bin/env bash
# Regenerate the bindings for *this platform* from the committed FreeRDP headers.
#
#   ./gen-bindings.sh            # rewrite src/bindings_<platform>.rs
#   ./gen-bindings.sh --check    # fail if that file is not what the headers say
#
# Why generated and committed rather than generated at build time: bindgen needs libclang, and a
# `-sys` crate whose entire selling point is that a consumer needs no C toolchain cannot then
# require an LLVM installation to build.
#
# ---------------------------------------------------------------------------------------------
# **Two files, not one, and this was measured rather than assumed.**
#
# libvpx-prebuilt commits a single bindings.rs for all three targets, and that is sound there:
# bindgen emits `c_int` and `c_char` as *aliases* which each target resolves for itself. FreeRDP
# is not like that. `winpr/wtypes.h` says:
#
#     #ifndef __APPLE__
#     typedef int32_t BOOL;
#     #else
#     #include <TargetConditionals.h>
#     #if OBJC_BOOL_IS_BOOL
#     typedef bool BOOL;
#     #else
#     typedef signed char BOOL;
#     #endif
#     #endif
#
# So `BOOL` is **four bytes on Linux and one byte on Apple** — measured: `pub type BOOL = i32`
# against `pub type BOOL = c_schar` from the same committed headers. It appears in 866 places in
# the generated file, including the return type of nearly every FreeRDP function and the
# signature of every callback the wrapper installs. A single file would be an ABI mismatch on one
# of the two platforms, and not the kind that fails to compile: it is the kind where a callback
# returns a byte into a slot the caller reads four bytes of.
#
# `winpr/pack.h` differs on `__APPLE__` too, for structure packing.
#
# Hence one file per platform family, selected by `cfg` in lib.rs, and each generated **on** that
# platform. Cross-generating with clang's `-target` would be neater but is not available: the
# Apple branch includes `<TargetConditionals.h>` and the Linux branch needs glibc's headers, so
# each side needs its own sysroot. CI runs this on macOS and on both Linux targets, which is what
# keeps both files honest — and asserts that the two Linux architectures agree, so that
# `bindings_linux.rs` is one file rather than two.
# ---------------------------------------------------------------------------------------------
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"
# shellcheck source-path=SCRIPTDIR
# shellcheck source=../../freerdp.env
. ../../freerdp.env

# Pinned, because bindgen's output is not stable across its own versions — field ordering, the
# shape of the generated enum constants and the layout tests have all changed between releases.
# An unpinned generator turns `--check` into a test of which bindgen the runner happened to
# install.
BINDGEN_VERSION=0.72.1

case "$(uname -s)" in
  Darwin) platform=apple ;;
  Linux) platform=linux ;;
  *)
    echo "this repository builds no archives for $(uname -s), so there are no bindings to make" >&2
    exit 1
    ;;
esac
out="src/bindings_${platform}.rs"

command -v bindgen >/dev/null 2>&1 || {
  echo "bindgen is not installed. cargo install bindgen-cli --version $BINDGEN_VERSION --locked" >&2
  exit 1
}
actual_version="$(bindgen --version | awk '{print $2}')"
[ "$actual_version" = "$BINDGEN_VERSION" ] || {
  echo "bindgen $actual_version is installed but this file is generated with $BINDGEN_VERSION" >&2
  echo "  cargo install bindgen-cli --version $BINDGEN_VERSION --locked --force" >&2
  exit 1
}

# rustfmt, required rather than nice to have — and required because of a measurement. bindgen
# runs rustfmt over its output if it can find one, and **carries on with a warning if it cannot**:
# the run in a `rust:1-trixie` container without the rustfmt component produced a complete,
# correct 30,000-declaration binding written as sixteen enormous lines. It compiles. It is also
# unreviewable, and it makes `--check` a comparison between a formatted file and an unformatted
# one, so the first machine without rustfmt turns the check permanently red for a reason that has
# nothing to do with the headers.
command -v rustfmt >/dev/null 2>&1 || {
  echo "rustfmt is not on PATH. bindgen would emit an unformatted file and only warn about it." >&2
  echo "  rustup component add rustfmt" >&2
  exit 1
}

# Which headers the bindings may describe, and it has to be a *file* filter rather than a name
# one. `--allowlist-function 'freerdp_.*'` would miss `gdi_init`, `graphics_register_pointer` and
# every `CLIPRDR_*` struct; a list of prefixes long enough to cover them would silently stop
# covering a renamed one. The file pattern says the same thing structurally: whatever FreeRDP and
# WinPR declare in their own installed headers, and nothing the platform's libc declares.
allowlist='.*/(freerdp3|winpr3)/.*'

generate() {
  # Layout checks kept, deliberately — no `--no-layout-tests`, and it matters more here than in a
  # small codec. `rdpPointer`, `rdpUpdate` and `rdpInput` carry explicit `paddingA[16-7]`-style
  # arrays whose arithmetic *is* the ABI: FreeRDP reserves slots so that a later release can add
  # a callback without moving the ones after it, and a Rust struct that computes that arithmetic
  # differently would install a callback at the wrong offset and be called for the wrong event.
  #
  # bindgen 0.72 emits these as `const _: () = { ["Size of X"][size_of::<X>() - N]; }`, which
  # fails at **compile** time rather than as a `#[test]` — better than a test, since a consumer
  # who never runs the test suite still cannot build against a struct that packs differently on
  # their target.
  #
  # `--default-enum-style consts` rather than rustified enums: FreeRDP's error codes and its
  # `FreeRDP_Settings_Keys_*` families are C enums its own API passes as integers, and several
  # are compared against values a future FreeRDP may extend. A Rust enum with an unlisted
  # discriminant is undefined behaviour; a constant is a number.
  #
  # The settings keys get a module each instead, and that is a readability decision with one
  # measurement behind it. `consts` flattens every enumerator to a top-level constant prefixed
  # with its enum's name, and FreeRDP's settings keys are already prefixed — so
  # `FreeRDP_RedirectClipboard` comes out as
  # `FreeRDP_Settings_Keys_Bool_FreeRDP_RedirectClipboard`, and there are about six hundred of
  # them. `--constified-enum-module` puts each family in a module, giving
  # `FreeRDP_Settings_Keys_Bool::FreeRDP_RedirectClipboard`, which is what the C reads like. It
  # keeps the constants-not-Rust-enums property that matters: these are still integers, so a key
  # a future FreeRDP adds cannot be undefined behaviour.
  #
  # `--rust-target` pinned for the same reason the bindgen version is, and it is the one flag
  # that decides this crate's MSRV: bindgen defaults to the newest Rust it knows about, and from
  # 1.82 it emits `unsafe extern "C" { … }` blocks which do not parse on an older compiler.
  bindgen wrapper.h \
    --rust-target 1.81 \
    --allowlist-file "$allowlist" \
    --default-enum-style consts \
    --constified-enum-module 'FreeRDP_Settings_Keys_.*' \
    --no-doc-comments \
    --raw-line "// @generated by gen-bindings.sh on $platform, from the FreeRDP $FREERDP_VERSION" \
    --raw-line "// headers in include/ — do not edit. Regenerate with bindgen $BINDGEN_VERSION." \
    --raw-line "//" \
    --raw-line "// There is a second file beside this one for the other platform, and that is not" \
    --raw-line "// duplication: winpr/wtypes.h makes \`BOOL\` a four-byte int32_t on Linux and a" \
    --raw-line "// one-byte signed char on Apple, so one file cannot describe both ABIs. See the" \
    --raw-line "// long note at the top of gen-bindings.sh." \
    --raw-line "//" \
    --raw-line "// \`--allowlist-file\` restricts this to items declared by FreeRDP's and WinPR's" \
    --raw-line "// own installed headers. Without it the output also carries whatever the" \
    --raw-line "// generating machine's libc declares, which is one platform's idea of it." \
    --raw-line "#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]" \
    --raw-line "#![allow(clippy::all)]" \
    -- -I include/freerdp3 -I include/winpr3
}

# Is what bindgen just wrote actually FreeRDP's API?
#
# bindgen exits 0 on plenty of ways of producing nothing useful — an allowlist that matched no
# file, a header it could not open, a clang that parsed the tree and found every declaration
# filtered out. All of them leave a syntactically valid Rust file containing the raw-line header
# and not much else, and `--check` would then happily certify it. So the output is measured
# rather than trusted: a handful of declarations that must be there, and a floor on the size.
assert_complete() {
  local file="$1" lines
  for declaration in \
    'pub fn freerdp_connect' \
    'pub fn freerdp_client_context_new' \
    'pub fn gdi_init' \
    'pub struct rdp_pointer' \
    'pub mod FreeRDP_Settings_Keys_Bool' \
    'pub type BOOL'; do
    grep -q "$declaration" "$file" || {
      echo "the generated bindings do not declare '$declaration'." >&2
      echo "  bindgen exited successfully, so this is an empty or filtered result rather than a" >&2
      echo "  parse failure — check the -I paths and the allowlist above." >&2
      return 1
    }
  done
  # And a floor, which is what catches the *formatting* going wrong rather than the content: an
  # unformatted binding of the same declarations is about sixteen very long lines.
  lines="$(wc -l < "$file")"
  [ "$lines" -gt 10000 ] || {
    echo "the generated bindings are only $lines lines. FreeRDP's surface is ~30,000 formatted," >&2
    echo "  so this is almost certainly rustfmt not having run." >&2
    return 1
  }
}

if [ "${1:-}" = "--check" ]; then
  [ -f "$out" ] || {
    echo "$out does not exist — run gen-bindings.sh on a $platform machine" >&2
    exit 1
  }
  # Compared as text rather than by regenerating in place and asking git: this has to work in a
  # CI job that has not necessarily checked out with a clean tree, and the diff is worth printing
  # either way.
  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT
  generate > "$tmp"
  assert_complete "$tmp"
  if diff -u "$out" "$tmp"; then
    echo "$out matches the committed FreeRDP $FREERDP_VERSION headers"
    # Said out loud, because a green check here covers *one* platform. The other file is checked
    # by the other platform's CI job, and nothing on this machine can speak for it.
    other=apple
    [ "$platform" = apple ] && other=linux
    echo "   note: src/bindings_${other}.rs is not checked here — that is the $other job's"
    exit 0
  fi
  echo "$out is stale — run gen-bindings.sh on a $platform machine" >&2
  exit 1
fi

# Generated beside the target and renamed onto it, rather than redirected straight into it: a
# `> "$out"` truncates the committed bindings *before* bindgen runs, so a failed generation — a
# missing header, a clang that cannot parse one — leaves the crate with an empty bindings file
# and no way back except git.
tmp="$(mktemp "$out.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
generate > "$tmp"
assert_complete "$tmp"
chmod 644 "$tmp"
mv "$tmp" "$out"
trap - EXIT
echo "wrote $out ($(wc -l < "$out" | tr -d ' ') lines from FreeRDP $FREERDP_VERSION, $platform ABI)"
