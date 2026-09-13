// Fetch the native libraries sucher loads at runtime, and stage them beside the
// binary this build produces (ADR 0022).
//
// Two libraries, same shape: libpdfium (PDF rendering, ADR 0015) and libduckdb
// (data files, ADR 0016). Neither is linked. Both are downloaded as a *pinned,
// checksum-verified* release asset for the build target, placed in OUT_DIR, and
// copied next to the binary, where `pdfium.rs` and `duckdyn.rs` resolve them at
// first use.
//
// Why sidecars rather than bytes in the executable: macOS charges for the whole
// image at `exec` rather than for the pages that execute, about 15 ms per MB, so
// a library that lives inside the binary costs startup on EVERY run, including
// `sucher note.md`, whether or not anything opens a PDF or a Parquet. Measured:
// a test binary carrying 40 MB no code path reads starts 0.63 s slower.
//
// `embed-pdfium` is the one exception, off by default. `cargo install` copies
// only the binary and can place no sidecar, so that build embeds libpdfium with
// `include_bytes!` and writes it to a cache dir on first use, paying the ~110 ms
// to stay self-contained.
//
// Every failure path is soft: an unsupported target, no network (offline builds,
// docs.rs), a missing `curl`, or a checksum mismatch just skips that library with
// a warning. PDF then falls back to poppler, and data files report the missing
// library like any other unopenable file. The build never hard-fails on account
// of either.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned bblanchon/pdfium-binaries release. Bump alongside the SHA-256 table.
const TAG: &str = "chromium/7961";

/// Pinned duckdb/duckdb release. Bump alongside [`duckdb_asset`]'s SHA-256s.
/// `duckdyn.rs` transcribes this version's C API signatures, so the two move
/// together.
const DUCKDB_TAG: &str = "v1.5.5";

/// For the build target: (asset stem, path of the lib inside the tarball,
/// destination file name, SHA-256 of the `.tgz`). `None` for targets we don't
/// ship a binary for.
fn target_asset() -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    Some(match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => (
            "pdfium-mac-arm64",
            "lib/libpdfium.dylib",
            "libpdfium.dylib",
            "1193a771e0bd934530afa3df73a0d44551d8f4078442e290054e6dd38ded960f",
        ),
        ("macos", "x86_64") => (
            "pdfium-mac-x64",
            "lib/libpdfium.dylib",
            "libpdfium.dylib",
            "17f069d7012ab83898ad5eddebd139b240f05d7411c220775d507a0e3e285536",
        ),
        ("linux", "x86_64") => (
            "pdfium-linux-x64",
            "lib/libpdfium.so",
            "libpdfium.so",
            "019665c8877d46fe65f625f80fd714ab07aac68554b0636acf2a2adf9288adb2",
        ),
        ("linux", "aarch64") => (
            "pdfium-linux-arm64",
            "lib/libpdfium.so",
            "libpdfium.so",
            "974107999784a438149605024475d42d80dd306799d90e1af5f6fa63f976455f",
        ),
        ("windows", "x86_64") => (
            "pdfium-win-x64",
            "bin/pdfium.dll",
            "pdfium.dll",
            "88276459349b291c41f10422dad0210f007c04d919c8fa56472b6b7c6406adf4",
        ),
        _ => return None,
    })
}

/// For the build target: (asset file name, the library's name inside the zip,
/// destination file name, SHA-256 of the `.zip`). `None` for targets DuckDB does
/// not publish a library for. macOS ships one universal asset for both arches.
fn duckdb_asset() -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    Some(match (os.as_str(), arch.as_str()) {
        ("macos", _) => (
            "libduckdb-osx-universal.zip",
            "libduckdb.dylib",
            "libduckdb.dylib",
            "7b5b8915cc382d0708636fe6385c0cdad5a61c9ff8ba2638b3e2141640783155",
        ),
        ("linux", "x86_64") => (
            "libduckdb-linux-amd64.zip",
            "libduckdb.so",
            "libduckdb.so",
            "1fb8ce388157d84a25abe685a8a2520bf00c00321821968e4bb398fd766e7abb",
        ),
        ("linux", "aarch64") => (
            "libduckdb-linux-arm64.zip",
            "libduckdb.so",
            "libduckdb.so",
            "abe4f6f005ee0b448a058322f4263584b4bd1b6faf7ab4637b79eeaf978f8e9c",
        ),
        ("windows", "x86_64") => (
            "libduckdb-windows-amd64.zip",
            "duckdb.dll",
            "duckdb.dll",
            "8375eb1fcf2212e8a0817950354815d4dde9dd383c2d9fa7b8975b71e278c1bd",
        ),
        ("windows", "aarch64") => (
            "libduckdb-windows-arm64.zip",
            "duckdb.dll",
            "duckdb.dll",
            "006f8df62957f640a100d673432a5b6f9a7002662822a4567ed06a436ee1d801",
        ),
        _ => return None,
    })
}

fn main() {
    // Register the cfg so `-D warnings` (unexpected_cfgs) stays happy whether or
    // not embedding succeeds. Must be emitted on every path.
    println!("cargo:rustc-check-cfg=cfg(pdfium_embedded)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SUCHER_PDFIUM_LIB");
    println!("cargo:rerun-if-env-changed=SUCHER_PDFIUM_NO_EMBED");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let skip = std::env::var_os("SUCHER_PDFIUM_NO_EMBED").is_some()
        || std::env::var_os("DOCS_RS").is_some();

    stage_pdfium(&out, skip);
    // Only with the `data` feature is there anything to load the library for.
    if std::env::var_os("CARGO_FEATURE_DATA").is_some() {
        stage_duckdb(&out, skip);
    }
}

/// Fetch libpdfium, stage it beside the binary, and embed it when the opt-in
/// feature asks for that.
fn stage_pdfium(out: &Path, skip: bool) {
    if skip {
        warn("pdfium fetch skipped (env); PDF will use the poppler fallback");
        return;
    }
    let Some((asset, member, libfile, sha)) = target_asset() else {
        warn("no prebuilt pdfium for this target; PDF will use the poppler fallback");
        return;
    };
    let dest = out.join(libfile);
    let url =
        format!("https://github.com/bblanchon/pdfium-binaries/releases/download/{TAG}/{asset}.tgz");
    match ensure_lib(&dest, out, &url, Archive::TarGz, member, asset, sha) {
        Ok(()) => {
            stage_beside_binary(out, &dest, libfile);
            // Only the opt-in build also carries the bytes inside the executable,
            // where they cost ~110 ms of startup whether or not a PDF is opened.
            if std::env::var_os("CARGO_FEATURE_EMBED_PDFIUM").is_some() {
                println!("cargo:rustc-cfg=pdfium_embedded");
                println!("cargo:rustc-env=SUCHER_PDFIUM_EMBEDDED={}", dest.display());
                println!("cargo:rustc-env=SUCHER_PDFIUM_LIBFILE={libfile}");
            }
        }
        Err(e) => warn(&format!(
            "could not obtain libpdfium ({e}); PDF will use the poppler fallback"
        )),
    }
}

/// Fetch libduckdb and stage it beside the binary. Never embedded: it is ~38 MB
/// of engine, which is precisely the startup cost ADR 0022 removed.
fn stage_duckdb(out: &Path, skip: bool) {
    if skip {
        warn("duckdb fetch skipped (env); data files will report a missing library");
        return;
    }
    let Some((asset, member, libfile, sha)) = duckdb_asset() else {
        warn("no prebuilt libduckdb for this target; data files will not open");
        return;
    };
    // A system copy is what `duckdyn::resolve_library_path` falls back to, and
    // Homebrew's own `duckdb` formula installs exactly that. Downloading 34 MB to
    // stage a second copy beside the binary would be pure duplication, so when
    // one is already there, do nothing and let the runtime resolver find it.
    if system_lib_dirs().iter().any(|d| d.join(libfile).is_file()) {
        return;
    }
    let dest = out.join(libfile);
    let url = format!("https://github.com/duckdb/duckdb/releases/download/{DUCKDB_TAG}/{asset}");
    match ensure_lib(&dest, out, &url, Archive::Zip, member, asset, sha) {
        Ok(()) => {
            thin_to_host_arch(&dest);
            stage_beside_binary(out, &dest, libfile);
        }
        Err(e) => warn(&format!(
            "could not obtain libduckdb ({e}); data files will report a missing library"
        )),
    }
}

/// Copy a fetched library next to the binary this build produces, so a plain
/// `cargo build` leaves `sucher` and its libraries side by side and the runtime
/// resolvers find them with no Makefile plumbing and no second copy of the
/// pinned version and checksum. Failure is not fatal, each library has a
/// documented absent-behaviour.
fn stage_beside_binary(out: &Path, src: &Path, libfile: &str) {
    if let Some(dir) = profile_dir(out) {
        let _ = std::fs::copy(src, dir.join(libfile));
    }
}

/// Where a system-installed copy of a library would be, matching the tail of
/// `duckdyn::resolve_library_path` (and `pdfium::resolve_library_path`).
fn system_lib_dirs() -> Vec<PathBuf> {
    match std::env::var("CARGO_CFG_TARGET_OS")
        .unwrap_or_default()
        .as_str()
    {
        "macos" => vec![
            PathBuf::from("/opt/homebrew/lib"),
            PathBuf::from("/usr/local/lib"),
        ],
        "windows" => Vec::new(),
        _ => vec![PathBuf::from("/usr/local/lib"), PathBuf::from("/usr/lib")],
    }
}

/// DuckDB ships one universal macOS library (~117 MB for two architectures).
/// Slice it to the host arch, halving what a packager installs. Best effort: no
/// `lipo`, a non-fat file, or any failure leaves the original in place, which
/// works either way.
fn thin_to_host_arch(lib: &Path) {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "macos" {
        return;
    }
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        _ => return,
    };
    let thin = lib.with_extension("thin");
    let ok = Command::new("lipo")
        .arg(lib)
        .args(["-thin", arch, "-output"])
        .arg(&thin)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && thin.is_file() {
        let _ = std::fs::rename(&thin, lib);
    } else {
        let _ = std::fs::remove_file(&thin);
    }
}

/// The profile output directory (`target/release`, `target/debug`, ...) that holds
/// the binary for this build, derived from OUT_DIR
/// (`<profile>/build/<pkg>-<hash>/out`). `None` if the layout is not that shape,
/// in which case the sidecar is simply not staged.
fn profile_dir(out: &Path) -> Option<PathBuf> {
    let build = out.parent()?.parent()?;
    if build.file_name()? != "build" {
        return None;
    }
    Some(build.parent()?.to_path_buf())
}

fn warn(msg: &str) {
    println!("cargo:warning=sucher: {msg}");
}

/// How a release asset is packed. pdfium-binaries ships `.tgz`, DuckDB `.zip`.
#[derive(Clone, Copy)]
enum Archive {
    TarGz,
    Zip,
}

/// Ensure `dest` holds the library: reuse a prior build, a local override, or a
/// vendored copy; otherwise download + verify + unpack the pinned asset.
#[allow(clippy::too_many_arguments)]
fn ensure_lib(
    dest: &Path,
    out: &Path,
    url: &str,
    kind: Archive,
    member: &str,
    asset: &str,
    sha: &str,
) -> Result<(), String> {
    if dest.is_file() {
        return Ok(()); // cached in OUT_DIR from an earlier build
    }
    let libfile = dest
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or("destination has no file name")?;
    // Local sources first (offline dev, CI cache): explicit override, then a
    // `vendor/<lib>/<file>` copy in the source tree.
    for src in local_candidates(libfile) {
        if src.is_file() {
            std::fs::copy(&src, dest).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    let archive = out.join(asset);
    download(url, &archive)?;
    verify_sha256(&archive, sha)?;
    match kind {
        Archive::TarGz => extract_member(&archive, member, dest)?,
        Archive::Zip => extract_zip_member(&archive, member, dest)?,
    }
    let _ = std::fs::remove_file(&archive);
    Ok(())
}

/// Local copies to prefer over a download, keyed by the library's file name so
/// one function serves both. `vendor/pdfium/` keeps working for pdfium, and
/// `vendor/duckdb/` is its counterpart.
fn local_candidates(libfile: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let (env_var, vendor) = if libfile.contains("duckdb") {
        ("SUCHER_DUCKDB_LIB", "vendor/duckdb")
    } else {
        ("SUCHER_PDFIUM_LIB", "vendor/pdfium")
    };
    if let Ok(p) = std::env::var(env_var) {
        v.push(PathBuf::from(p));
    }
    if let Ok(root) = std::env::var("CARGO_MANIFEST_DIR") {
        v.push(PathBuf::from(root).join(vendor).join(libfile));
    }
    v
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args(["-sSL", "--fail", "--max-time", "180", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| format!("curl not runnable: {e}"))?;
    if !status.success() {
        return Err(format!("curl exited {status} for {url}"));
    }
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let got: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != expected {
        return Err(format!("sha256 mismatch (got {got}, want {expected})"));
    }
    Ok(())
}

/// Pull `member` out of a zip by its BASE NAME, so a flat archive and one that
/// nests the library under a directory both work without a second pin to keep
/// in step.
fn extract_zip_member(zip: &Path, member: &str, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut ar = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    for i in 0..ar.len() {
        let mut entry = ar.by_index(i).map_err(|e| e.to_string())?;
        let matches = entry
            .enclosed_name()
            .and_then(|p| p.file_name().map(|f| f == member))
            .unwrap_or(false);
        if matches {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            std::fs::write(dest, &buf).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err(format!("member {member} not found in {}", zip.display()))
}

fn extract_member(tgz: &Path, member: &str, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(tgz).map_err(|e| e.to_string())?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    for entry in ar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        let name = path.to_string_lossy();
        if name.trim_start_matches("./") == member {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            std::fs::write(dest, &buf).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err(format!("member {member} not found in {}", tgz.display()))
}
