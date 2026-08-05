# shellcheck shell=bash
# Fetch and verify the pinned FreeRDP and OpenSSL sources. Sourced, not run — by build.sh, which
# compiles them, and by sync-prebuilt.sh, which needs the licence files out of the FreeRDP tree.
#
# One copy of this rather than two, because it is where the repository's central promise lives:
# the bytes are checksummed *before* anything is compiled, so a tree that is not the pinned one
# never reaches a compiler or an artifact other projects link. Two implementations of that rule
# is one too many.
#
# Tarballs with sha256 pins rather than libvpx-prebuilt's git clone with a commit assertion, and
# that is an upgrade rather than a difference of taste. Both projects publish real release
# artifacts with checksums beside them, so the pin can cover the exact bytes that get compiled
# instead of a tree that git then has to be trusted to reproduce. It is also 40 MB of download
# instead of a clone of two large histories.

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

# Download $1 to $2 unless $2 already exists. Staged under a pid-suffixed name and renamed, so a
# killed run cannot leave a half-written file that the next run then happily checksums — the
# checksum would fail, but with "the pin is wrong" rather than "the download was interrupted",
# and those want different responses.
_fetch() {
  local url="$1" out="$2"
  [ -f "$out" ] && return 0
  local staging="$out.tmp$$"
  echo ">> downloading $(basename "$out")"
  curl -sSL --fail --max-time 600 --retry 3 -o "$staging" "$url" || {
    rm -f "$staging"
    echo "cannot download $url" >&2
    return 1
  }
  mv "$staging" "$out"
}

# Download $1, verify it against $2, and unpack it into build/$3 unless that already exists.
#
# The checksum runs on **every** call, not only after a download: the cached tarball is the copy
# most likely to be wrong, because it survives across runs and nothing else ever looks at it
# again. A corrupt one otherwise surfaces as a compiler error three hundred files in.
_fetch_verify_unpack() {
  local url="$1" expected="$2" dir="$3" tarball="$4"

  mkdir -p build
  _fetch "$url" "build/$tarball" || return 1

  local actual
  actual="$(sha256_of "build/$tarball")"
  [ "$actual" = "$expected" ] || {
    echo "checksum mismatch for build/$tarball" >&2
    echo "  the pin says $expected" >&2
    echo "  the file is  $actual" >&2
    echo "  (delete build/$tarball to re-download, or fix the pin in freerdp.env)" >&2
    return 1
  }
  echo ">> $tarball sha256 $actual — matches the pin"

  if [ ! -d "build/$dir" ]; then
    echo ">> unpacking $tarball"
    # Into a staging directory and renamed, for the same reason the download is: an interrupted
    # `tar x` otherwise leaves a *partial source tree* that looks complete enough to configure.
    rm -rf "build/$dir.unpack"
    mkdir -p "build/$dir.unpack"
    tar xzf "build/$tarball" -C "build/$dir.unpack"
    # One top-level directory per tarball, whatever it happens to be called — both of these name
    # it after the version, but relying on that would make a renamed upstream directory a
    # confusing failure rather than an obvious one.
    local inner
    inner="$(find "build/$dir.unpack" -mindepth 1 -maxdepth 1 -type d)"
    # Non-empty *and* one line. `wc -l` on nothing counts 1 here, because a here-string always ends
    # in a newline — so a tarball that unpacked no directory at all otherwise passes this check and
    # reaches `mv` with an empty source, which fails as a usage error naming neither tarball.
    [ -n "$inner" ] && [ "$(wc -l <<<"$inner")" -eq 1 ] || {
      echo "$tarball does not hold exactly one top-level directory:" >&2
      printf '  %s\n' "$inner" >&2
      rm -rf "build/$dir.unpack"
      return 1
    }
    mv "$inner" "build/$dir"
    rm -rf "build/$dir.unpack"
  fi
}

# Leaves the FreeRDP tree at build/freerdp-$FREERDP_VERSION. Idempotent; callers use that path
# directly.
ensure_freerdp() {
  _fetch_verify_unpack "$FREERDP_URL" "$FREERDP_SHA256" \
    "freerdp-${FREERDP_VERSION}" "freerdp-${FREERDP_VERSION}.tar.gz"
}

# Leaves the OpenSSL tree at build/openssl-$OPENSSL_VERSION.
ensure_openssl() {
  _fetch_verify_unpack "$OPENSSL_URL" "$OPENSSL_SHA256" \
    "openssl-${OPENSSL_VERSION}" "openssl-${OPENSSL_VERSION}.tar.gz"
}
