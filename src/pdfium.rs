// Runtime pdfium backend (ADR 0015) — Chrome's PDF engine.
//
// Shelling to poppler's `pdftocairo` re-parses the whole document and re-inits a
// cold process *per page*, and cairo resamples scanned-image pages in scalar
// software: a full-page scan takes ~4.5 s. pdfium renders the same page in ~30 ms
// and hands back an RGBA bitmap in-process — no PNG round-trip, no subprocess.
//
// libpdfium is loaded at *runtime* (never linked): `make` fetches the pinned
// dylib and places it beside the binary; this module resolves it at first use.
// pdfium's document/bindings handles are `!Send` and its library must be
// initialised exactly once per process, so all rendering runs on a single
// dedicated service thread that owns the `Pdfium` instance and caches the
// most-recently-opened document (parsing is ~0.2 ms, but this also avoids
// re-reading the file's bytes when paging). Callers fall back to poppler
// (`pdf::render_page`) whenever the library is absent or a specific render fails.

use image::DynamicImage;
#[cfg(pdfium_embedded)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;

/// One render request handed to the service thread, with a private reply channel.
struct Job {
    path: String,
    page: usize,
    width: u32,
    reply: Sender<Result<DynamicImage, String>>,
}

/// The process-wide service: `Some(sender)` once libpdfium loaded successfully,
/// `None` if it could not be found or bound (→ callers use poppler).
static SERVICE: OnceLock<Option<Sender<Job>>> = OnceLock::new();

fn service() -> Option<&'static Sender<Job>> {
    SERVICE.get_or_init(init_service).as_ref()
}

/// Whether pdfium is usable this process (library found and bound).
pub fn available() -> bool {
    service().is_some()
}

/// Render `page` (0-based) of `path` to an RGBA image `width` px wide via pdfium.
/// Blocks until the service thread replies. `Err` when pdfium is unavailable or
/// the render fails — the caller is expected to fall back to poppler.
pub fn render(path: &str, page: usize, width: u32) -> Result<DynamicImage, String> {
    let svc = service().ok_or("pdfium unavailable")?;
    let (tx, rx) = mpsc::channel();
    svc.send(Job {
        path: path.to_string(),
        page,
        width,
        reply: tx,
    })
    .map_err(|_| "pdfium service gone".to_string())?;
    rx.recv()
        .map_err(|_| "pdfium service dropped".to_string())?
}

/// Spawn the service thread and wait for it to confirm the library bound. Returns
/// the job sender, or `None` if the library is missing or fails to load.
fn init_service() -> Option<Sender<Job>> {
    let lib = resolve_library_path()?;
    let (tx, rx) = mpsc::channel::<Job>();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
    std::thread::Builder::new()
        .name("pdfium".into())
        .spawn(move || service_loop(lib, rx, ready_tx))
        .ok()?;
    // Block only on the one-time bind (cold ~260 ms, warm ~2 ms), off the UI.
    match ready_rx.recv() {
        Ok(true) => Some(tx),
        _ => None,
    }
}

fn service_loop(lib: PathBuf, rx: mpsc::Receiver<Job>, ready: Sender<bool>) {
    use pdfium_render::prelude::*;

    let bindings = match Pdfium::bind_to_library(&lib) {
        Ok(b) => b,
        Err(_) => {
            let _ = ready.send(false);
            return;
        }
    };
    let pdfium = Pdfium::new(bindings);
    let _ = ready.send(true);

    // Cache the last-opened document. Its lifetime borrows `pdfium`, which outlives
    // this loop, so both live as locals here rather than in a (self-referential)
    // struct.
    let mut cached_path: Option<String> = None;
    let mut cached_doc: Option<PdfDocument> = None;

    while let Ok(job) = rx.recv() {
        if cached_path.as_deref() != Some(job.path.as_str()) {
            cached_doc = None;
            cached_path = None;
            match pdfium.load_pdf_from_file(&job.path, None) {
                Ok(doc) => {
                    cached_doc = Some(doc);
                    cached_path = Some(job.path.clone());
                }
                Err(e) => {
                    let _ = job.reply.send(Err(format!("pdfium load: {e:?}")));
                    continue;
                }
            }
        }
        let res = match &cached_doc {
            Some(doc) => render_one(doc, job.page, job.width),
            None => Err("pdfium: no document".to_string()),
        };
        let _ = job.reply.send(res);
    }
}

/// Rasterise one page to `width` px wide, preserving aspect (mirrors poppler's
/// `-scale-to-x width`).
fn render_one(
    doc: &pdfium_render::prelude::PdfDocument,
    page: usize,
    width: u32,
) -> Result<DynamicImage, String> {
    use pdfium_render::prelude::*;

    let pages = doc.pages();
    if page >= pages.len() as usize {
        return Err("pdfium: page out of range".to_string());
    }
    let page = pages.get(page as u16).map_err(|e| format!("{e:?}"))?;
    let pw = page.width().value;
    if pw <= 0.0 {
        return Err("pdfium: non-positive page width".to_string());
    }
    // Clamp the scale so a pathological page size can't request an enormous raster.
    let factor = (width as f32 / pw).clamp(0.05, 20.0);
    let cfg = PdfRenderConfig::new().scale_page_by_factor(factor);
    let bitmap = page
        .render_with_config(&cfg)
        .map_err(|e| format!("{e:?}"))?;
    Ok(bitmap.as_image())
}

/// Platform file name of the pdfium shared library.
fn lib_file_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "libpdfium.dylib"
    }
    #[cfg(target_os = "windows")]
    {
        "pdfium.dll"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "libpdfium.so"
    }
}

/// Locate libpdfium, in priority order:
///
/// 1. `$SUCHER_PDFIUM_LIB` (explicit full path — used for dev / overrides),
/// 2. beside the running executable (where `make install` copies it),
/// 3. common system library directories.
///
/// Returns `None` if not found — the caller then uses poppler.
fn resolve_library_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SUCHER_PDFIUM_LIB") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let file = lib_file_name();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.to_path_buf());
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/opt/homebrew/lib"));
        dirs.push(PathBuf::from("/usr/local/lib"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        dirs.push(PathBuf::from("/usr/local/lib"));
        dirs.push(PathBuf::from("/usr/lib"));
    }
    for d in dirs {
        let cand = d.join(file);
        if cand.is_file() {
            return Some(cand);
        }
    }
    // Last resort: the copy embedded in the binary by build.rs, materialised to a
    // cache dir. This is what makes `cargo install sucher` self-contained; an
    // external copy above still wins so a user can override with a newer library.
    #[cfg(pdfium_embedded)]
    if let Some(p) = materialize_embedded() {
        return Some(p);
    }
    None
}

/// The pdfium library embedded at build time (present only when build.rs fetched
/// it for this target). Written to a cache file on first use because `dlopen`
/// needs a real path, not memory.
#[cfg(pdfium_embedded)]
const EMBEDDED_LIB: &[u8] = include_bytes!(env!("SUCHER_PDFIUM_EMBEDDED"));
#[cfg(pdfium_embedded)]
const EMBEDDED_LIBFILE: &str = env!("SUCHER_PDFIUM_LIBFILE");

/// Write the embedded library to `<cache>/sucher/` (once) and return its path.
/// The file name carries a content stamp so a library update lands a new file and
/// never loads a stale one; writes are atomic (temp + rename) so two first-run
/// processes can't tear each other's file.
#[cfg(pdfium_embedded)]
fn materialize_embedded() -> Option<PathBuf> {
    let stamp = fnv1a_hex(EMBEDDED_LIB);
    let ext = Path::new(EMBEDDED_LIBFILE)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let name = if ext.is_empty() {
        format!("libpdfium-{stamp}")
    } else {
        format!("libpdfium-{stamp}.{ext}")
    };
    let dir = dirs::cache_dir()?.join("sucher");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(&name);
    if path.is_file()
        && std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) == EMBEDDED_LIB.len() as u64
    {
        return Some(path); // already extracted
    }
    let tmp = dir.join(format!(".{stamp}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, EMBEDDED_LIB).ok()?;
    // rename is atomic within a dir; a lost race just means the other copy wins.
    let _ = std::fs::rename(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    path.is_file().then_some(path)
}

/// 8-hex-char FNV-1a of `bytes` — a cheap content stamp for the cache file name
/// (not a security hash; the bytes are already trusted, compiled into the binary).
#[cfg(pdfium_embedded)]
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", h as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_file_name_matches_platform() {
        let name = lib_file_name();
        #[cfg(target_os = "macos")]
        assert_eq!(name, "libpdfium.dylib");
        #[cfg(target_os = "windows")]
        assert_eq!(name, "pdfium.dll");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(name, "libpdfium.so");
    }

    #[test]
    fn env_override_is_honoured_when_the_file_exists() {
        // A real file the override points at is returned verbatim; a non-existent
        // one is ignored so resolution falls through to the search path.
        let dir = std::env::temp_dir().join(format!("sucher-pdfium-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("libpdfium.dylib");
        std::fs::write(&f, b"not a real library").unwrap();

        // SAFETY: single-threaded test; no other thread reads the env concurrently.
        unsafe { std::env::set_var("SUCHER_PDFIUM_LIB", &f) };
        assert_eq!(resolve_library_path().as_deref(), Some(f.as_path()));

        unsafe { std::env::set_var("SUCHER_PDFIUM_LIB", dir.join("nope.dylib")) };
        assert_ne!(
            resolve_library_path().as_deref(),
            Some(dir.join("nope.dylib").as_path())
        );

        unsafe { std::env::remove_var("SUCHER_PDFIUM_LIB") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    // End-to-end render through the real service thread. Ignored by default; run
    // with a build that embedded pdfium (the default) or `SUCHER_PDFIUM_LIB` set:
    //   cargo test --bin sucher -- --ignored --test-threads=1 renders_a_real_pdf
    #[test]
    #[ignore]
    fn renders_a_real_pdf() {
        assert!(
            available(),
            "pdfium unavailable — build with embedding or set SUCHER_PDFIUM_LIB"
        );
        let img = render("samples/sample.pdf", 0, 800).expect("render page 0");
        assert_eq!(img.width(), 800, "should raster to the requested width");
        assert!(img.height() > 0);
        // Second page from the resident (cached) document.
        let p2 = render("samples/sample.pdf", 1, 800).expect("render page 1");
        assert_eq!(p2.width(), 800);
    }
}
