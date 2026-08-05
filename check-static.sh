#!/usr/bin/env bash
# Assert that a binary carries FreeRDP and OpenSSL inside it rather than expecting to find them.
#
#   ./check-static.sh target/release/freerdp-e2e
#
# Three questions, and they fail in different directions:
#
#   positive — is FreeRDP actually *in* there? FreeRDP compiles its own version string in, so
#             finding `3.30.0` in the file's bytes says yes, and says *which* — a stronger answer
#             than grepping for a symbol, which does not tell one build of a library from another.
#   negative — is there a *dynamic* dependency on FreeRDP, WinPR or OpenSSL as well or instead?
#             This is the one that passes every test on the build machine and then fails on a slim
#             runtime image or a machine without Homebrew, which is precisely the failure this
#             repository exists to remove. OpenSSL is the sharpest case: a binary that links the
#             host's libssl works until it meets a host with a different one, and the symptom is
#             a TLS handshake failing rather than a missing file.
#   the C++ runtime — build.sh measures whether the archives need one and records the answer in
#             the MANIFEST. FreeRDP is C, and the one option that would drag C++ in
#             (`WITH_UNICODE_BUILTIN=OFF`, via ICU) is off, so the answer is `none` — a property
#             worth keeping. When the MANIFEST says so, this requires the finished binary to have
#             no libstdc++/libc++ dependency either.
#
# Run in CI on every target. A binary that links a system FreeRDP by accident behaves identically
# to a correct one until it is copied somewhere else.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

bin="${1:?usage: ./check-static.sh <binary>}"
[ -f "$bin" ] || { echo "no such file: $bin" >&2; exit 1; }

# shellcheck source=freerdp.env
. "$here/freerdp.env"

fail=0

echo ">> $bin"

# `grep -a`: treat the executable as text, which is portable in a way `nm` and `strings` are not.
if grep -aq "$FREERDP_VERSION" "$bin"; then
  echo "   ok    FreeRDP $FREERDP_VERSION's version string is compiled in"
else
  echo "   FAIL  no '$FREERDP_VERSION' string in the binary — is FreeRDP really linked, and is" >&2
  echo "         it the pinned version?" >&2
  fail=1
fi

case "$(uname -s)" in
  Darwin) deps="$(otool -L "$bin" | tail -n +2 || true)" ;;
  *)      deps="$(ldd "$bin" 2>/dev/null || true)" ;;
esac

# "Found nothing" is the answer these greps exist to give, so they accept exit 1 and nothing else:
# exit 2 is grep saying it could not do the search, and a search that did not happen looks exactly
# like a clean binary if its status is thrown away. Same rule as build.sh's `matches`.
#
# The answer goes into a global rather than out of a `$(…)`, because the failure has to be able to
# end the *script*: an `exit` inside a command substitution ends only the subshell, so a check
# written that way reports the grep it could not run as a binary with nothing wrong with it.
matched=''
find_deps() {
  local status=0
  matched="$(printf '%s\n' "$deps" | grep -iE "$1")" || status=$?
  [ "$status" -le 1 ] || {
    echo "   FAIL  grep failed ($status) looking for '$1' — the dependency is unknown, not absent" >&2
    exit 1
  }
}

# One pattern covering all four, because they fail for the same reason and a binary that got any
# of them dynamically got them from somewhere this repository did not pin.
find_deps 'libfreerdp|libwinpr|libssl|libcrypto'
if dynamic="$matched" && [ -n "$dynamic" ]; then
  echo "   FAIL  dynamic dependency on a library this repository ships statically:" >&2
  printf '           %s\n' "$dynamic" >&2
  fail=1
else
  echo "   ok    no dynamic libfreerdp/libwinpr/libssl/libcrypto"
fi

# What did build.sh measure for this target? Looked up rather than assumed, and skipped rather
# than guessed when there is no MANIFEST to read — a check that invents its own expectation is
# worse than one that says it did not run.
manifest=""
for candidate in "$here"/dist/*/MANIFEST "$here"/crates/freerdp-prebuilt-sys/prebuilt/*/MANIFEST; do
  [ -f "$candidate" ] || continue
  manifest="$candidate"
done

if [ -n "$manifest" ]; then
  cxx_runtime="$(sed -n 's/^cxx_runtime //p' "$manifest")"
  echo "   note  $(basename "$(dirname "$manifest")") MANIFEST says cxx_runtime: ${cxx_runtime:-<absent>}"
  if [ "$cxx_runtime" = "none" ]; then
    find_deps 'libstdc\+\+|libc\+\+'
    if cxx_deps="$matched" && [ -n "$cxx_deps" ]; then
      echo "   FAIL  the archives need no C++ runtime, but the binary links one:" >&2
      printf '           %s\n' "$cxx_deps" >&2
      echo "         Nothing in this repository should emit -lstdc++/-lc++; check whether" >&2
      echo "         WITH_UNICODE_BUILTIN went OFF and pulled ICU in." >&2
      fail=1
    else
      echo "   ok    no C++ runtime dependency, as the measurement predicted"
    fi
  fi
else
  echo "   note  no MANIFEST found — skipping the C++ runtime check"
fi

exit "$fail"
