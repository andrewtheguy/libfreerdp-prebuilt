// Find the prebuilt FreeRDP for this target and emit the link flags. What this build script does
// *not* do is the point of the crate: no cmake, no configure, no pkg-config, no OpenSSL to
// install, no vendored source tree, no `cc`, and no libclang.
//
// Three places an archive set can come from, tried in this order:
//
//   1. `FREERDP_PREBUILT_DIR` — a prefix you built or unpacked yourself. Used as-is.
//   2. `prebuilt/<target>/` next to this file — what `./build.sh` + `./sync-prebuilt.sh` leave
//      behind, and gitignored, because a committed `.a` is one nobody can tell apart from the
//      one CI made.
//   3. the repository's **latest** GitHub release, downloaded once per machine into
//      `$CARGO_HOME/freerdp-prebuilt/`.
//
// (3) is what makes a fresh clone of a consuming project build with nothing installed; (1) is
// what makes it work with no network at all.
//
// Two things get hashed on the way in, and neither hash is committed to this repository:
//
//   - the downloaded `.tar.gz`, against the `SHA256SUMS` asset the release job generates from
//     the archives it just built and publishes beside them;
//   - every extracted `.a`, against the `sha256(lib/…)` lines in the MANIFEST inside the archive
//     — on every resolution path, not just the download.
//
// Both are **corruption** checks, not tamper checks: each list travels with the files it covers,
// so whoever could replace one could replace both. They earn their place because corruption is
// what actually happens — a truncated download, a cache half-written by a killed build, an `.a`
// overwritten by hand — and each of those otherwise surfaces as a page of undefined symbols
// rather than one sentence naming the file.
//
// The pins that constrain somebody *other than us* are the third-party ones: `freerdp.env` holds
// a sha256 for each of FreeRDP's and OpenSSL's release tarballs, and `source.sh` refuses to
// unpack anything else.
//
// **The link order and the system libraries are read out of the MANIFEST, not hardcoded here.**
// FreeRDP is five archives whose dependencies form a DAG, and rustc has no `--start-group`, so
// getting the order wrong is a page of undefined symbols. `build.sh` measures which system
// libraries and macOS frameworks the archives actually need — and then proves the answer by
// linking a probe against exactly that set — so this file emits a measurement rather than a
// guess that would go stale the first time an option changed.
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=FREERDP_PREBUILT_DIR");

    // Before anything is hashed for real — see the note on the function.
    check_sha256_implementation();

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let target = std::env::var("TARGET").unwrap();
    let version = freerdp_env(&manifest_dir, "FREERDP_VERSION");
    println!("cargo:rustc-env=FREERDP_PREBUILT_VERSION={version}");

    let (prefix, provenance) = resolve(&manifest_dir, &target, &version);
    let lib_dir = prefix.join("lib");
    assert!(
        lib_dir.is_dir(),
        "no lib/ in {} (from {provenance})\n\nFREERDP_PREBUILT_DIR must name a prefix \
         *containing* lib/, not the lib/ directory itself.",
        prefix.display(),
    );
    let manifest = verify_libraries(&prefix, &lib_dir, &provenance);

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    for lib in manifest.link_order() {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    for lib in manifest.system_libs() {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    for framework in manifest.frameworks() {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    // For a consumer compiling its own C against the same headers, via the `DEP_FREERDP_INCLUDE`
    // that `links = "freerdp"` exposes. Two roots, separated by a comma, because FreeRDP installs
    // its own and WinPR's side by side and both are needed to parse a single `freerdp/freerdp.h`.
    let include = manifest_dir.join("include");
    println!(
        "cargo:include={},{}",
        include.join("freerdp3").display(),
        include.join("winpr3").display()
    );
    println!("cargo:rerun-if-changed={}", lib_dir.display());

    // `cargo:info`, not `cargo:warning`: this is the normal case, and a warning on every build of
    // every consumer is noise that teaches people to ignore warnings. Visible under
    // `cargo build -vv`, which is where the README says to look.
    println!("cargo:info=FreeRDP {version} linked statically from {provenance} ({target})");
    for key in ["openssl", "channels", "cpu_floor", "system_libs", "frameworks"] {
        if let Some(value) = manifest.get(key) {
            println!("cargo:info=FreeRDP {key} {value}");
        }
    }
    if provenance == "FREERDP_PREBUILT_DIR" {
        // This one earns a warning: the archives came from outside the repo, so nothing checked
        // their version, their channel set, or which OpenSSL they were built against.
        println!(
            "cargo:warning=FreeRDP from FREERDP_PREBUILT_DIR ({}) — unverified",
            prefix.display()
        );
    }
}

/// The MANIFEST that `build.sh` writes beside the archives, or the fallback for a prefix that has
/// none.
struct Manifest {
    text: Option<String>,
}

/// What to link, and in what order, when there is no MANIFEST to read it from.
///
/// Only `FREERDP_PREBUILT_DIR` can get here, and `main` warns that nothing about it was checked.
/// The order is the one `build.sh` records and CI proves by linking a probe: the client archive
/// calls into the core, the core calls into WinPR, and all three call into OpenSSL. No
/// back-edges, which is what makes a single pass enough.
const FALLBACK_LINK_ORDER: &[&str] = &["freerdp-client3", "freerdp3", "winpr3", "ssl", "crypto"];

impl Manifest {
    fn get(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key} ");
        self.text.as_deref()?.lines().find_map(|line| line.strip_prefix(&prefix)).map(str::trim)
    }

    /// The archives, in link order, as rustc wants them: `-lfreerdp-client3` -> `freerdp-client3`.
    fn link_order(&self) -> Vec<String> {
        match self.get("link_order") {
            Some(line) => line
                .split_whitespace()
                .map(|flag| {
                    flag.strip_prefix("-l")
                        .unwrap_or_else(|| {
                            panic!("the MANIFEST's link_order holds '{flag}', which is not a -l flag")
                        })
                        .to_string()
                })
                .collect(),
            None => {
                println!(
                    "cargo:warning=no link_order in the MANIFEST — falling back to the canonical \
                     order, which may not match these archives"
                );
                FALLBACK_LINK_ORDER.iter().map(|s| s.to_string()).collect()
            }
        }
    }

    /// Measured by `build.sh` from the archives' undefined symbols, then proved by linking a
    /// probe against exactly this set. `none` is a real answer — it is what macOS records, where
    /// libm, libdl and libpthread are all part of libSystem and Rust's own std already links it.
    fn system_libs(&self) -> Vec<String> {
        self.list("system_libs", &["m", "dl", "pthread", "rt"])
    }

    /// macOS only, and empty everywhere else. WinPR reaches into CoreFoundation and Foundation
    /// for its unicode and path handling.
    fn frameworks(&self) -> Vec<String> {
        self.list("frameworks", &[])
    }

    fn list(&self, key: &str, fallback: &[&str]) -> Vec<String> {
        match self.get(key) {
            Some("none") => Vec::new(),
            Some(line) => line.split_whitespace().map(str::to_string).collect(),
            None => {
                if !fallback.is_empty() {
                    // The conservative direction: an unnecessary `-lpthread` on a glibc target is
                    // a no-op, while a missing one is a link failure with no hint in it.
                    println!(
                        "cargo:warning=no {key} line in the MANIFEST — linking {} to be safe",
                        fallback.join(" ")
                    );
                }
                let apple = std::env::var("TARGET").unwrap_or_default().contains("apple");
                if apple { Vec::new() } else { fallback.iter().map(|s| s.to_string()).collect() }
            }
        }
    }
}

/// Hash every archive on disk and require each to be what the MANIFEST beside them says was
/// built. Returns the MANIFEST, since every later step wants to read a line out of it.
///
/// Runs on every resolution path, not just the download: the cache is the copy most likely to be
/// wrong, because it survives across builds and nothing else ever looks at it again. Hashing
/// twenty megabytes costs a few tens of milliseconds and turns a corrupt file into a sentence
/// instead of a page of undefined symbols.
fn verify_libraries(prefix: &Path, lib_dir: &Path, provenance: &str) -> Manifest {
    let text = match std::fs::read_to_string(prefix.join("MANIFEST")) {
        Ok(text) => text,
        // A prefix from outside this repository is the only one allowed to arrive without a
        // MANIFEST, and main() already warns that nothing about it has been checked.
        Err(_) if provenance == "FREERDP_PREBUILT_DIR" => return Manifest { text: None },
        Err(e) => panic!(
            "no readable MANIFEST in {} (from {provenance}): {e}\n\nEvery archive this \
             repository publishes carries one.",
            prefix.display(),
        ),
    };

    let mut checked = 0;
    for line in text.lines() {
        // `sha256(lib/libwinpr3.a) <hex>` — one line per archive, so a release that grew or lost
        // a library is covered without this file being told about it.
        let Some(rest) = line.strip_prefix("sha256(lib/") else { continue };
        let Some((name, hash)) = rest.split_once(") ") else { continue };
        let path = lib_dir.join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "the MANIFEST in {} lists lib/{name}, which cannot be read: {e}",
                prefix.display()
            )
        });
        let actual = sha256_hex(&bytes);
        let expected = hash.trim();
        // A hand-written panic rather than assert_eq!, whose own "left: … right: …" tail would
        // repeat both hashes underneath a message that already lays them out readably.
        if actual != expected {
            panic!(
                "\n\n{} does not match the MANIFEST beside it (from {provenance}).\n\
                 \x20 MANIFEST says {expected}\n\
                 \x20 the file is  {actual}\n\n\
                 The archive is corrupt or was modified after it was built. Delete the directory \
                 above and build again.\n",
                path.display(),
            );
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "the MANIFEST in {} lists no archives — it is not one build.sh wrote",
        prefix.display()
    );
    println!("cargo:info=FreeRDP {checked} archives match the MANIFEST");
    Manifest { text: Some(text) }
}

/// SHA-256 (FIPS 180-4), by hand.
///
/// The alternatives are a crate — which every consumer would then compile, in a `-sys` crate
/// whose entire selling point is that it builds nothing — or shelling out to a different tool per
/// platform (`shasum`, `sha256sum`, `certutil`), none of which is guaranteed to exist wherever
/// cargo does. Fifty lines with a test vector beats both.
fn sha256_hex(bytes: &[u8]) -> String {
    #[rustfmt::skip]
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pad to a multiple of 64 bytes: a 1 bit, zeros, then the length in bits, big-endian.
    let mut msg = Vec::with_capacity(bytes.len() + 72);
    msg.extend_from_slice(bytes);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&(bytes.len() as u64 * 8).to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (word, src) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(add);
        }
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

fn resolve(manifest_dir: &Path, target: &str, version: &str) -> (PathBuf, String) {
    if let Some(dir) = std::env::var_os("FREERDP_PREBUILT_DIR") {
        return (PathBuf::from(dir), "FREERDP_PREBUILT_DIR".into());
    }

    let name = prebuilt_dir(target);
    let local = manifest_dir.join("prebuilt").join(name);
    if local.join("lib").is_dir() {
        return (local, format!("prebuilt/{name}"));
    }

    let cached = cache_root().join(version).join(name);
    if cached.join("lib").is_dir() {
        return (cached, format!("cache/{version}/{name}"));
    }

    (fetch(manifest_dir, name, version, &cached), format!("latest release asset for {name}"))
}

/// Download the latest release's archive for one target and unpack it into the cache.
///
/// `releases/latest/download/…` rather than a pinned tag: a tag written in here is a tag that has
/// to be updated in here. The asset name carries the FreeRDP version, so a release of a
/// *different* version cannot satisfy the URL — it 404s naming the version rather than quietly
/// returning the wrong library.
fn fetch(manifest_dir: &Path, name: &str, version: &str, cached: &Path) -> PathBuf {
    assert!(
        std::env::var("CARGO_NET_OFFLINE").as_deref() != Ok("true"),
        "FreeRDP for {name} is not cached and cargo is offline. Run ./build.sh {name} && \
         ./sync-prebuilt.sh, or set FREERDP_PREBUILT_DIR to a prefix containing lib/."
    );

    let repo = freerdp_env(manifest_dir, "PREBUILT_REPO");
    let asset = format!("freerdp-{version}-{name}.tar.gz");
    let base = format!("https://github.com/{repo}/releases/latest/download");

    // Staged under a pid-suffixed name so two cargo builds racing here cannot read each other's
    // half-written tarball. The loser of the race throws its copy away below.
    let staging = cached.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).expect("cannot create the cache directory");
    let tarball = staging.join(&asset);
    let sums = staging.join("SHA256SUMS");

    println!("cargo:info=fetching {base}/{asset}");
    if !curl(&format!("{base}/{asset}"), &tarball) {
        panic!("cannot download {base}/{asset}");
    }
    // Both URLs resolve `latest` independently, so a release published between these two requests
    // would give a mismatch below rather than a wrong library — which is the right way round for
    // a race this unlikely.
    if !curl(&format!("{base}/SHA256SUMS"), &sums) {
        panic!(
            "cannot download {base}/SHA256SUMS\n\nEvery release publishes one beside the \
             archives. If the latest release predates that, upgrade this crate or set \
             FREERDP_PREBUILT_DIR to a prefix you built yourself."
        );
    }
    verify_download(&sums, &asset, &tarball);

    // `tar` rather than a Rust tar crate: it is present on macOS, on every Linux image that can
    // run cargo, and in System32 on Windows 10 1803 and later, and a build dependency here would
    // be one every consumer compiles.
    run(Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&staging));
    std::fs::remove_file(&tarball).ok();
    std::fs::remove_file(&sums).ok();

    std::fs::create_dir_all(cached.parent().unwrap()).ok();
    if std::fs::rename(&staging, cached).is_err() {
        // Either another build populated the cache first — fine, use theirs — or the rename
        // genuinely failed, which the caller's `lib/` check will report.
        let _ = std::fs::remove_dir_all(&staging);
    }
    cached.to_path_buf()
}

/// The downloaded tarball against the SHA256SUMS published beside it on the same release.
///
/// Same standing as the MANIFEST check and for the same reason — the list travels with the files
/// it covers — so this catches a truncated or mangled download, not a dishonest release. It
/// replaces relying on `tar` to notice: gzip's CRC does catch corruption, but it reports it as
/// "unexpected end of file" from a program the user did not know was running, which is a worse
/// sentence than this one.
fn verify_download(sums: &Path, asset: &str, tarball: &Path) {
    let text = std::fs::read_to_string(sums).expect("cannot read the downloaded SHA256SUMS");
    // coreutils writes `<hex>  <name>`, and `sha256sum ./*.tar.gz` would prefix the name with
    // `./` — accepted here so that how the release job spelled its glob cannot break every
    // consumer's build.
    let expected = text
        .lines()
        .find_map(|line| {
            let (hash, rest) = line.split_once(char::is_whitespace)?;
            (rest.trim().trim_start_matches("./") == asset).then_some(hash)
        })
        .unwrap_or_else(|| {
            panic!("SHA256SUMS on the latest release does not list {asset}:\n{text}")
        });

    let bytes = std::fs::read(tarball).expect("cannot read the downloaded archive");
    let actual = sha256_hex(&bytes);
    if actual != expected {
        panic!(
            "\n\n{asset} does not match the SHA256SUMS published beside it.\n\
             \x20 SHA256SUMS says {expected}\n\
             \x20 the download is {actual}\n\n\
             The download is corrupt, or a release was published while it was in flight. Try \
             again.\n"
        );
    }
    println!("cargo:info=FreeRDP {asset} matches SHA256SUMS ({actual})");
}

/// `curl` one URL into one file, reporting whether it worked rather than dying, so that each
/// caller can say what a failure means.
fn curl(url: &str, out: &Path) -> bool {
    Command::new("curl")
        .args(["-sSL", "--fail", "--max-time", "600", "--retry", "3", "-o"])
        .arg(out)
        .arg(url)
        .status()
        .unwrap_or_else(|e| panic!("cannot run curl: {e}"))
        .success()
}

fn run(cmd: &mut Command) {
    let program = cmd.get_program().to_string_lossy().into_owned();
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("{program} failed: {status}"),
        Err(e) => panic!("cannot run {program}: {e}"),
    }
}

/// `$CARGO_HOME/freerdp-prebuilt/`, so the download happens once per machine rather than once per
/// project — and so the many Docker builds that already cache `~/.cargo` get it for free with no
/// extra configuration.
fn cache_root() -> PathBuf {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")))
        .expect("neither CARGO_HOME nor a home directory is set");
    home.join("freerdp-prebuilt")
}

/// The repo's target names are not Rust triples — they name *artifacts*, and several triples map
/// to one artifact.
fn prebuilt_dir(target: &str) -> &'static str {
    // **musl does not share the glibc archive here**, which is where this diverges from
    // libvpx-prebuilt. FreeRDP's WinPR reaches well past libm: `dlopen`, `getpwuid_r`,
    // `getaddrinfo` and the NSS machinery behind them are exactly the surface where a glibc-built
    // static archive linked into a musl binary compiles and then misbehaves at run time. Refusing
    // is the honest answer until somebody builds and tests a musl target.
    match target {
        "aarch64-apple-darwin" => "macos-arm64",
        "x86_64-unknown-linux-gnu" => "linux-x86_64",
        "aarch64-unknown-linux-gnu" => "linux-aarch64",
        "x86_64-apple-darwin" => panic!(
            "no prebuilt FreeRDP for Intel macOS: the macOS artifact is arm64. Set \
             FREERDP_PREBUILT_DIR to a prefix holding your own archives, or add the target to \
             build.sh."
        ),
        t if t.contains("musl") => panic!(
            "no prebuilt FreeRDP for {t}. These archives are built against glibc, and WinPR's \
             use of dlopen and the NSS resolver is exactly where a glibc archive in a musl \
             binary misbehaves at run time rather than at link time. Add a musl target to \
             build.sh, or set FREERDP_PREBUILT_DIR."
        ),
        t if t.contains("windows") => panic!(
            "no prebuilt FreeRDP for {t}: this repository builds no Windows archives, and the \
             OpenSSL half would need its own toolchain setup. Set FREERDP_PREBUILT_DIR to a \
             prefix holding your own, or add the target to build.sh — which is real work, not a \
             line in a case statement."
        ),
        other => panic!(
            "no prebuilt FreeRDP for {other}. Supported: aarch64-apple-darwin, \
             x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu. Set FREERDP_PREBUILT_DIR to a \
             prefix holding your own archives for anything else."
        ),
    }
}

/// Read one setting out of `freerdp.env`, which is where the shell build keeps the same values —
/// parsed rather than duplicated, so the two halves of this repository cannot disagree about
/// which FreeRDP this is or where its archives live.
fn freerdp_env(manifest_dir: &Path, key: &str) -> String {
    let env_file = manifest_dir.join("../../freerdp.env");
    println!("cargo:rerun-if-changed={}", env_file.display());
    let text = std::fs::read_to_string(&env_file).unwrap_or_else(|e| {
        panic!("cannot read {}: {e} — is this crate outside its repository?", env_file.display())
    });
    let prefix = format!("{key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {key} in freerdp.env"))
        .trim()
        .to_string()
}

/// Check the hash against FIPS 180-4's published vectors, on every build.
///
/// Not a `#[cfg(test)] mod tests`, and that is the whole point of this comment. Cargo builds a
/// build script as a *binary it runs*, never as a test target — so a `#[test]` in this file is
/// compiled by nothing and run by nothing, and `cargo test` reports it as zero tests passing
/// while looking exactly like success. Fifty lines of hand-written hash guarded by a test that
/// does not exist is worse than fifty lines with no test at all, because the second kind gets
/// read carefully.
///
/// So it runs unconditionally, before the first hash that decides anything. Three short vectors
/// plus one megabyte of `'a'` — the long one is what exercises multi-block padding, the part most
/// likely to be wrong and least likely to show up on short inputs. The whole check costs about a
/// millisecond, once per build of this crate.
fn check_sha256_implementation() {
    for (input, expected) in [
        (Vec::new(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        (b"abc".to_vec(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".to_vec(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
        (vec![b'a'; 1_000_000], "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"),
    ] {
        let actual = sha256_hex(&input);
        assert_eq!(
            actual,
            expected,
            "this build script's SHA-256 is wrong on a published test vector ({} bytes of \
             input). Every archive integrity check in this file is meaningless until that is \
             fixed.",
            input.len(),
        );
    }
}
