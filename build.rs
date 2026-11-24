use std::path::{Path, PathBuf};
use std::process::Command;

/// Static library produced by the `FluidAudioBridge` product.
const LIB_FILE: &str = "libFluidAudioBridge.a";

fn main() {
    // Tell Cargo to rerun if Swift files change
    println!("cargo:rerun-if-changed=swift/");
    println!("cargo:rerun-if-changed=Package.swift");
    println!("cargo:rerun-if-changed=Package.resolved");
    println!("cargo:rerun-if-env-changed=FLUIDAUDIO_SWIFT_BUILD_DIR");
    println!("cargo:rerun-if-env-changed=FLUIDAUDIO_SWIFT_SHARED_CACHE");
    println!("cargo:rerun-if-env-changed=CI");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

    // The Swift configuration stays `release` for every Cargo profile, so the linked
    // library is the same one the stock build script produced.
    let config = "release";

    // Build the Swift package first to get FluidAudio dependency.
    //
    // SwiftPM scratch path: by default one per-user cache shared by every Cargo target
    // dir, worktree and command (check, clippy, build, test), so the ~40 s Swift build
    // runs once instead of once per `OUT_DIR`. See `shared_scratch_dir`.
    let private_dir = out_dir.join("swift-build");
    let shared_dir = shared_scratch_dir(&manifest_dir);

    let swift_build_dir = match shared_dir {
        Some(dir) => {
            println!(
                "cargo:warning=Building Swift package (shared cache {})...",
                dir.display()
            );
            match swift_build(&manifest_dir, &dir, config) {
                Ok(()) => dir,
                Err(err) => {
                    // Cache unusable (unwritable, corrupt, ...): do exactly what the stock
                    // build script does instead of failing the whole build.
                    println!(
                        "cargo:warning=Shared Swift build failed ({err}); \
                         retrying in {}",
                        private_dir.display()
                    );
                    swift_build(&manifest_dir, &private_dir, config)
                        .unwrap_or_else(|e| panic!("Swift package build failed: {e}"));
                    private_dir
                }
            }
        }
        None => {
            println!("cargo:warning=Building Swift package...");
            swift_build(&manifest_dir, &private_dir, config)
                .unwrap_or_else(|e| panic!("Swift package build failed: {e}"));
            private_dir
        }
    };

    // Find the built library
    let lib_path = swift_build_dir.join(config);
    let lib_file = lib_path.join(LIB_FILE);
    if !lib_file.is_file() {
        panic!(
            "Swift build succeeded but {} is missing",
            lib_file.display()
        );
    }
    // A shared cache can be cleaned while an older target dir still points at it. Rerun
    // (a no-op if the cache is intact, a rebuild if it is gone) whenever the library
    // is missing or has been rebuilt.
    println!("cargo:rerun-if-changed={}", lib_file.display());

    // Link the Swift library
    println!("cargo:rustc-link-search=native={}", lib_path.display());
    println!("cargo:rustc-link-lib=static=FluidAudioBridge");

    // Link Apple frameworks
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=AVFoundation");
    println!("cargo:rustc-link-lib=framework=CoreML");
    println!("cargo:rustc-link-lib=framework=Accelerate");
    println!("cargo:rustc-link-lib=framework=Metal");
    println!("cargo:rustc-link-lib=framework=MetalPerformanceShaders");

    // Link Swift runtime
    println!("cargo:rustc-link-lib=dylib=swiftCore");

    // Link C++ standard library (needed for FastClusterWrapper.cpp in FluidAudio)
    println!("cargo:rustc-link-lib=c++");
}

/// Runs `swift build -c <config>` for this package with the given SwiftPM scratch path.
fn swift_build(manifest_dir: &Path, scratch: &Path, config: &str) -> Result<(), String> {
    std::fs::create_dir_all(scratch)
        .map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;

    let status = Command::new("swift")
        .args([
            "build",
            "-c",
            config,
            "--build-path",
            scratch.to_str().ok_or("non-UTF-8 scratch path")?,
            // Package.resolved pins FluidAudio; never re-resolve (or rewrite it).
            "--only-use-versions-from-resolved-file",
        ])
        .current_dir(manifest_dir)
        .status()
        .map_err(|e| format!("failed to run swift build: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("swift build exited with {status}"))
    }
}

/// Scratch directory shared between Cargo target dirs, or `None` for a private one.
///
/// Base directory: `FLUIDAUDIO_SWIFT_BUILD_DIR` (absolute path) when set, otherwise
/// `$HOME/Library/Caches/fluidaudio-rs`. `FLUIDAUDIO_SWIFT_SHARED_CACHE=0` turns sharing
/// off (SwiftPM output stays under `OUT_DIR`, as in the stock build script).
///
/// CI runners (`CI` set, as GitHub Actions does) keep the stock behaviour unless
/// `FLUIDAUDIO_SWIFT_BUILD_DIR` is set explicitly: they cache the Cargo target dir, which
/// must therefore contain the Swift output, and release builds stay hermetic.
///
/// The directory used is `<base>/<key>`, where the key hashes everything that decides
/// what SwiftPM produces: the package location (a git checkout path contains the
/// revision), the contents of `Package.swift` / `Package.resolved`, and the Swift
/// toolchain (`swift --version`). A different revision or compiler therefore never
/// reuses another one's objects. Concurrent users of the same directory are serialised
/// by SwiftPM's own lock on the scratch path.
fn shared_scratch_dir(manifest_dir: &Path) -> Option<PathBuf> {
    if matches!(
        env_nonempty("FLUIDAUDIO_SWIFT_SHARED_CACHE").as_deref(),
        Some("0" | "false" | "off" | "no")
    ) {
        return None;
    }

    let explicit = env_nonempty("FLUIDAUDIO_SWIFT_BUILD_DIR");
    if explicit.is_none()
        && env_nonempty("CI").is_some_and(|v| !matches!(v.as_str(), "0" | "false"))
    {
        return None;
    }

    let base = match explicit {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            assert!(
                dir.is_absolute(),
                "FLUIDAUDIO_SWIFT_BUILD_DIR must be an absolute path, got {}",
                dir.display()
            );
            dir
        }
        None => PathBuf::from(env_nonempty("HOME")?)
            .join("Library")
            .join("Caches")
            .join("fluidaudio-rs"),
    };

    let toolchain = Command::new("swift").arg("--version").output().ok()?;
    if !toolchain.status.success() || toolchain.stdout.is_empty() {
        return None;
    }

    let mut key = Fnv1a::new();
    key.update(b"fluidaudio-rs swift scratch v1");
    key.update(manifest_dir.to_string_lossy().as_bytes());
    key.update(&toolchain.stdout);
    for file in ["Package.swift", "Package.resolved"] {
        key.update(&std::fs::read(manifest_dir.join(file)).unwrap_or_default());
    }
    Some(base.join(format!("{:016x}", key.finish())))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// 64-bit FNV-1a. `DefaultHasher` is not guaranteed stable across Rust releases, and a
/// changing key would silently orphan the cache; this one never changes.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    /// Hashes the length first so adjacent inputs cannot run together.
    fn update(&mut self, bytes: &[u8]) {
        for b in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
