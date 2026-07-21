// Fetch + embed libpdfium so `cargo install sucher` gets the fast PDF path with
// no extra steps (ADR 0015).
//
// `cargo install` copies only the compiled binary, so a sidecar library or a
// `make` step can't reach those users. Instead we download the *pinned,
// checksum-verified* pdfium shared library for the build target, place it in
// OUT_DIR, and let the crate `include_bytes!` it — the binary carries its own
// engine and writes it to a cache dir on first use (see `src/pdfium.rs`).
//
// Every failure path is soft: an unsupported target, no network (offline builds,
// docs.rs), a missing `curl`, or a checksum mismatch just skips embedding with a
// warning, and the crate falls back to poppler at runtime. So the build never
// hard-fails on account of pdfium.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned bblanchon/pdfium-binaries release. Bump alongside the SHA-256 table.
const TAG: &str = "chromium/7961";

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

fn main() {
    // Register the cfg so `-D warnings` (unexpected_cfgs) stays happy whether or
    // not embedding succeeds. Must be emitted on every path.
    println!("cargo:rustc-check-cfg=cfg(pdfium_embedded)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SUCHER_PDFIUM_LIB");
    println!("cargo:rerun-if-env-changed=SUCHER_PDFIUM_NO_EMBED");

    if std::env::var_os("SUCHER_PDFIUM_NO_EMBED").is_some() || std::env::var_os("DOCS_RS").is_some()
    {
        warn("pdfium embedding skipped (env); PDF will use the poppler fallback");
        return;
    }

    let Some((asset, member, libfile, sha)) = target_asset() else {
        warn("no prebuilt pdfium for this target; PDF will use the poppler fallback");
        return;
    };

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dest = out.join(libfile);

    match ensure_lib(&dest, &out, asset, member, libfile, sha) {
        Ok(()) => {
            println!("cargo:rustc-cfg=pdfium_embedded");
            println!("cargo:rustc-env=SUCHER_PDFIUM_EMBEDDED={}", dest.display());
            println!("cargo:rustc-env=SUCHER_PDFIUM_LIBFILE={libfile}");
        }
        Err(e) => warn(&format!(
            "could not obtain libpdfium ({e}); PDF will use the poppler fallback"
        )),
    }
}

fn warn(msg: &str) {
    println!("cargo:warning=sucher: {msg}");
}

/// Ensure `dest` holds the pdfium library: reuse a prior build, a local override,
/// or a vendored copy; otherwise download + verify + unpack the pinned tarball.
fn ensure_lib(
    dest: &Path,
    out: &Path,
    asset: &str,
    member: &str,
    libfile: &str,
    sha: &str,
) -> Result<(), String> {
    if dest.is_file() {
        return Ok(()); // cached in OUT_DIR from an earlier build
    }
    // Local sources first (offline dev, CI cache): explicit override, then a
    // `vendor/pdfium/<lib>` copy in the source tree.
    for src in local_candidates(libfile) {
        if src.is_file() {
            std::fs::copy(&src, dest).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    let url =
        format!("https://github.com/bblanchon/pdfium-binaries/releases/download/{TAG}/{asset}.tgz");
    let tgz = out.join(format!("{asset}.tgz"));
    download(&url, &tgz)?;
    verify_sha256(&tgz, sha)?;
    extract_member(&tgz, member, dest)?;
    let _ = std::fs::remove_file(&tgz);
    Ok(())
}

fn local_candidates(libfile: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("SUCHER_PDFIUM_LIB") {
        v.push(PathBuf::from(p));
    }
    if let Ok(root) = std::env::var("CARGO_MANIFEST_DIR") {
        v.push(PathBuf::from(root).join("vendor/pdfium").join(libfile));
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
