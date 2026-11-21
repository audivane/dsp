fn main() {
    println!("cargo:rerun-if-changed=src/pitch_bridge.cpp");
    println!("cargo:rerun-if-changed=third_party/signalsmith-stretch/signalsmith-stretch.h");
    println!("cargo:rerun-if-changed=third_party/signalsmith-linear/stft.h");
    println!("cargo:rerun-if-changed=third_party/signalsmith-linear/fft.h");

    cc::Build::new()
        .cpp(true)
        .std("c++14")
        .include("third_party")
        .file("src/pitch_bridge.cpp")
        .compile("audivane_pitch_bridge");

    build_environment();
}

/// The build's compiler, target and FFT crate versions, for the filter
/// fingerprint tests' environment header (`tests/filter_fingerprints.rs`),
/// recorded when the crate is built rather than read from the tools on
/// the runner's PATH. The engine does not use them.
fn build_environment() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let verbose = std::process::Command::new(&rustc)
        .arg("-vV")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let field = |key: &str| {
        verbose
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map_or_else(|| "unknown".to_string(), |v| v.trim().to_string())
    };
    let version = verbose
        .lines()
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_string();
    let lock = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let lock = std::fs::read_to_string(&lock).unwrap_or_default();
    let crate_version = |name: &str| {
        let mut lines = lock.lines();
        while let Some(l) = lines.next() {
            if l == format!("name = \"{name}\"")
                && let Some(v) = lines.next().and_then(|v| v.strip_prefix("version = "))
            {
                return v.trim_matches('"').to_string();
            }
        }
        "unknown".to_string()
    };
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "unknown".into());
    for (key, value) in [
        ("AVDSP_BUILD_RUSTC", version),
        ("AVDSP_BUILD_RUSTC_COMMIT", field("commit-hash:")),
        ("AVDSP_BUILD_LLVM", field("LLVM version:")),
        ("AVDSP_BUILD_TARGET", env("TARGET")),
        (
            "AVDSP_BUILD_TARGET_FEATURES",
            env("CARGO_CFG_TARGET_FEATURE"),
        ),
        ("AVDSP_BUILD_PROFILE", env("PROFILE")),
        ("AVDSP_BUILD_OPT_LEVEL", env("OPT_LEVEL")),
        ("AVDSP_BUILD_RUSTFFT", crate_version("rustfft")),
        ("AVDSP_BUILD_REALFFT", crate_version("realfft")),
    ] {
        println!("cargo:rustc-env={key}={value}");
    }
}
