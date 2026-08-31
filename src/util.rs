// Small shared formatting helpers used across viewers (the directory browser's
// list/preview, and the "no viewer" metadata line). Kept here so there is one
// implementation rather than per-module copies that could drift.

use quick_xml::events::{BytesRef, BytesText};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// Hard cap on the bytes any decoder reads from a single untrusted source
/// (ADR 0009). 32 MiB is generous for a real document — far larger than any
/// hand-written HTML page, Word body, or slide part — yet small enough that a
/// bounded parse stays sub-second and a decompression bomb inflates at most this
/// much before we stop and report honestly instead of OOM-ing.
pub const MAX_DECODE_BYTES: usize = 32 * 1024 * 1024;

/// Hard cap on bytes read for a directory-browser *preview* of a delimited-text
/// file (csv/tsv) (ADR 0009). A preview only ever shows the first handful of
/// rows, so 1 MiB holds far more than can be displayed. Unlike a full open, a
/// preview may legitimately show only a prefix of a huge file: the reader takes
/// at most this many bytes and parses what it got rather than erroring.
pub const MAX_PREVIEW_BYTES: usize = 1024 * 1024;

/// Hard cap on bytes inflated while *listing* an archive (ADR 0009). Larger than
/// [`MAX_DECODE_BYTES`] because a legitimate `.tar`/`.tar.gz` of source or media
/// easily exceeds 32 MiB and listing must stream through every member's bytes to
/// reach the next header (a gzip stream can't be seeked). 256 MiB lists the vast
/// majority of real archives in full while still bounding a gzip bomb's CPU to a
/// one-shot, cached ~second; past it the listing is truncated *with an explicit
/// marker row* — never silently (see `tar_entries`).
pub const MAX_ARCHIVE_INFLATE: usize = 256 * 1024 * 1024;

/// Maximum image dimension (px, per axis) any decoder will accept (ADR 0009).
/// Far above any real display need (a 4K screen is ~4000 px wide) yet small
/// enough that a tiny file *claiming* enormous dimensions cannot force a huge
/// allocation on decode; `image`'s default 512 MiB `max_alloc` is left in place.
pub const MAX_IMAGE_DIM: u32 = 20_000;

/// Read up to `max` bytes from `reader` into a `String`, returning `Err` when the
/// source exceeds `max` (we read `max + 1` and check the length) or is not valid
/// UTF-8. Bounds both memory and parse time for untrusted/compressed input: a
/// decompression bomb inflates at most `max + 1` bytes here before we stop, and a
/// parser is never handed a silently truncated half-document (ADR 0009).
pub fn read_to_string_capped<R: Read>(reader: R, max: usize) -> Result<String, String> {
    let mut buf = Vec::new();
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() > max {
        return Err(format!(
            "input exceeds {} preview/parse limit",
            human_size(max as u64)
        ));
    }
    String::from_utf8(buf).map_err(|_| "input is not valid UTF-8".to_string())
}

/// `image::Limits` bounding decode to [`MAX_IMAGE_DIM`] on each axis, keeping the
/// crate's default allocation ceiling. Applied to every `ImageReader`/decoder so
/// the browser preview and an interactive open share the same guard (ADR 0009).
pub fn image_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIM);
    limits.max_image_height = Some(MAX_IMAGE_DIM);
    limits
}

/// Open an image reader that decodes by *content*, not by file extension.
///
/// `image::ImageReader::open` picks its decoder from the path's extension, so a
/// real JPEG saved as `foo.png` (common with AI image tools that stamp C2PA
/// metadata) fails to decode. `with_guessed_format` sniffs the magic bytes and
/// overrides the extension, so misnamed-but-valid images preview correctly.
/// Pixel limits (ADR 0009) are applied before the caller decodes. Sucher still
/// *classifies* by extension (ADR 0001 D1) — this only governs decoding once a
/// file is already routed as a raster image.
pub fn open_image_reader(
    path: &Path,
) -> io::Result<image::ImageReader<io::BufReader<std::fs::File>>> {
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    reader.limits(image_limits());
    Ok(reader)
}

/// Content-sniffed image dimensions, mirroring [`open_image_reader`]: reads the
/// header only, so it is cheap and works for a misnamed image where the
/// extension-based `image::image_dimensions` would error.
pub fn image_dimensions(path: &Path) -> io::Result<(u32, u32)> {
    open_image_reader(path)?
        .into_dimensions()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Wall-clock ceiling for a *one-shot* poppler/ffmpeg subprocess (ADR 0009 item
/// 4). Generous for a big PDF page render or a media probe, yet bounds a hang: a
/// malicious file that wedges the tool would otherwise occupy the poster
/// worker's single in-flight raster slot forever, starving every later preview
/// (permanent spinner) and leaking the child. Not applied to the video player's
/// long-lived streaming ffmpeg, which is meant to run for the whole playback.
pub const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `cmd` to completion but no longer than `timeout`, capturing stdout and
/// stderr into an [`Output`] (ADR 0009 item 4). On timeout the child is killed
/// and reaped and an [`ErrorKind::TimedOut`] error is returned; callers map that
/// to the same graceful "no preview"/degraded path as any other spawn failure.
///
/// Deadlock avoidance: stdout and stderr are each drained on their own thread
/// *while* the child runs, so a tool that emits more than a pipe buffer (~64 KiB)
/// — `pdftotext -` dumping a large document, or ffmpeg writing a full rawvideo
/// frame to stdout — cannot wedge itself by blocking on a full pipe that we only
/// read after `wait`. We poll `try_wait` on a short sleep instead of blocking in
/// `wait`, so the deadline is enforced even for a child that never exits; after a
/// kill the pipes reach EOF and the reader threads join cleanly.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let err_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The kill EOFs the pipes; join so the reader threads finish
                    // rather than leak before we report the timeout.
                    let _ = out_h.join();
                    let _ = err_h.join();
                    return Err(io::Error::new(ErrorKind::TimedOut, "subprocess timed out"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Make `path` safe to pass as a positional argument to a subprocess tool
/// (poppler/ffmpeg) that would parse a leading `-` as an option (ADR 0009 / S4).
/// An absolute path (starting `/`) is returned unchanged; anything else is
/// prefixed with `./` so it can never begin with `-` and be misread as an option
/// — e.g. `sucher -x.pdf` yields `./-x.pdf`. A path already starting with `./` is
/// left as-is to avoid a redundant `././` (still correct, just tidier). Not shell
/// injection (no shell is used) — this guards direct invocation and globs.
pub fn cmd_path_arg(path: &str) -> String {
    if path.starts_with('/') || path.starts_with("./") {
        path.to_string()
    } else {
        format!("./{path}")
    }
}

/// Hand a **local file the user is already viewing** to the OS's default
/// application ("open in native app"). Unlike [`is_safe_url`] — which gates
/// *untrusted* link targets embedded in a document (ADR 0009 / S5) — the path
/// here is one the user explicitly selected or opened in sucher, so no scheme
/// allow-list applies; the file's own default handler is what "open externally"
/// means. Still guards the `-`-leading case ([`cmd_path_arg`]) so the path is
/// never misread as an option, and spawns detached (never blocks the TUI, never
/// waits on the child) so returning to sucher is instant. Best-effort: a missing
/// opener binary is silently ignored, matching [`open_url`](crate::tui).
pub fn open_in_native_app(path: &str) {
    let arg = cmd_path_arg(path);
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(arg).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(arg).spawn();
    // `rundll32 …FileProtocolHandler` opens via the default handler without a
    // `cmd` re-parse that a crafted filename could exploit (mirrors `open_url`).
    #[cfg(target_os = "windows")]
    let _ = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", &arg])
        .spawn();
}

/// Whether `url` (a link target from an *untrusted* document) is one we are
/// willing to hand to the OS opener (ADR 0009 / S5). Accepts only `http://`,
/// `https://`, and `mailto:` (scheme matched case-insensitively) and rejects any
/// `-`-leading target; a `file://`, `javascript:`, or custom-scheme link is
/// refused so `open`/`xdg-open` never acts on it.
pub fn is_safe_url(url: &str) -> bool {
    if url.starts_with('-') {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}

/// Hard cap on the bytes of text sucher will hand to the terminal for the system
/// clipboard (ADR 0017 D4). Terminals bound the length of an OSC 52 payload and
/// the ceiling is neither standardised nor discoverable: figures around 100 KB
/// are common and several implementations are far stricter, so the only safe
/// posture is to stay well under the smallest figure anyone reports. 64 KiB of
/// text becomes ~87 KB once base64 has expanded it by 4/3, which still sits
/// under the common ~100 KB figure, and it is three orders of magnitude more
/// than the feature actually needs: a `Y` on a thousand marked entries with
/// 64-character absolute paths is ~64 KB. Past the cap the yank is refused with
/// a named error and nothing is written, because a truncated payload would put
/// half a path on the clipboard and the user would paste it without noticing.
/// That is ADR 0009's "an honest error, never a silent truncation" pointed
/// outward.
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

/// Ask the terminal to put `text` on the **system** clipboard using OSC 52, the
/// escape sequence a terminal answers on the user's behalf (ADR 0017 D4). This
/// is the outbound sibling of [`open_in_native_app`]: both reach an OS facility
/// on the strength of something the user just did, and both are deliberately
/// separate entry points rather than one general "talk to the outside" helper
/// (ADR 0014). OSC 52 is used in preference to a clipboard crate because it
/// costs no dependency and, being a sequence the *terminal* interprets, it puts
/// the text on the clipboard of the machine the human is sitting at even when
/// sucher itself is running over ssh, which is the same reasoning that put the
/// kitty graphics and text-sizing protocols in this tool.
///
/// **`Ok` does not mean the clipboard changed.** OSC 52 is fire-and-forget: the
/// sequence has no reply, and a terminal is free to ignore it, as many do by
/// default for the obvious reason that a program which can write the clipboard
/// silently is a program that can overwrite whatever was on it (tmux needs
/// `set -g set-clipboard on`, and some terminals require a setting or refuse
/// outright). `Ok(())` therefore means exactly "the sequence was written to
/// stdout and flushed", and callers must not phrase their status message as a
/// confirmation. Wording that stays honest: `yanked 3 paths via OSC 52` or
/// `sent 3 paths to the terminal clipboard`, never `copied to clipboard`.
///
/// Text over [`MAX_CLIPBOARD_BYTES`] is refused rather than truncated; see that
/// constant for why the cap exists and where it sits.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let seq = osc52_sequence(text)?;
    // Straight to stdout with an explicit flush, and *not* through
    // `crossterm::execute!` as the mouse-capture guard in `dir.rs` does. That
    // macro exists to render crossterm's typed commands (`EnableMouseCapture`
    // and friends) into whatever the platform needs; OSC 52 has no such command,
    // so going through it would mean wrapping our own literal bytes in
    // `style::Print` and gaining nothing but indirection over the bytes we most
    // want to read at the call site. The file's other raw sequences (the OSC 11
    // background query in `config.rs`, the OSC 66 sizing probe in `plain.rs`)
    // already write themselves this way, so this matches the house pattern for
    // "a sequence crossterm has no name for".
    //
    // The flush is not tidiness. The browser calls this from inside a ratatui
    // alternate screen with raw mode on, and returns to a draw loop that may not
    // touch stdout for a while; a sequence still sitting in the buffer has done
    // nothing at all, and the status line would already be claiming otherwise.
    let mut out = io::stdout().lock();
    out.write_all(&seq).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

/// Build the exact OSC 52 byte sequence for `text`, or refuse it for being past
/// [`MAX_CLIPBOARD_BYTES`]. Split out from [`copy_to_clipboard`] so that
/// everything decided (the framing, the selection, the encoding, the size
/// refusal) is pure and asserted on byte-for-byte in tests, leaving only the
/// write itself impure and therefore untested rather than untestable.
///
/// The shape is `ESC ] 52 ; c ; <base64> BEL`. The `c` field names the target
/// selection, and `c` is *clipboard*: the one a plain paste (Cmd/Ctrl-V) reads.
/// The alternatives (`p` primary, `s` select, `0`-`7` cut buffers) are X11-era
/// selections that most users never paste from and that macOS has no notion of,
/// so for "the user pressed yank and expects to paste it anywhere", `c` is the
/// only selection that means what they meant.
///
/// Nothing from `text` can escape the sequence, and that is a property of the
/// encoding rather than an assumption about the input: base64 emits only
/// `A-Za-z0-9+/=`, so a path containing an ESC, a BEL, an ST, or an entire
/// forged OSC command survives as ordinary alphabet characters and cannot
/// terminate this sequence early or start a new one. Sucher therefore never has
/// to sanitise a filename on the way out.
fn osc52_sequence(text: &str) -> Result<Vec<u8>, String> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(format!(
            "selection is {}, past the {} the terminal clipboard accepts",
            human_size(text.len() as u64),
            human_size(MAX_CLIPBOARD_BYTES as u64)
        ));
    }
    let mut seq = Vec::with_capacity(text.len().div_ceil(3) * 4 + 8);
    seq.extend_from_slice(b"\x1b]52;c;");
    seq.extend_from_slice(base64_encode(text.as_bytes()).as_bytes());
    // BEL terminates the string. ST (`ESC \`) is the other legal terminator, but
    // BEL is the one every OSC 52 implementation accepts, and it is what the
    // rest of sucher's OSC traffic uses (`config.rs`'s OSC 11 query).
    seq.push(0x07);
    Ok(seq)
}

/// Encode `bytes` as standard base64 (RFC 4648: the `A-Za-z0-9+/` alphabet with
/// `=` padding). Written out rather than taken from a crate, because thirty
/// lines of arithmetic is not worth a dependency in a tool that ships its own
/// offline guarantee; `ipynb.rs` already carries the decoding direction for the
/// same reason. PURE, and unit-tested against the RFC's vectors.
///
/// It encodes **bytes**, never chars: input arrives as `&[u8]`, so a caller
/// passing `text.as_bytes()` gets its UTF-8 encoded a byte at a time, which is
/// what a terminal on the far end of an ssh session will decode back.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // Pack up to three bytes into a 24-bit register, zero-filling whatever a
        // short final chunk is missing, then read it back out as four 6-bit
        // groups. The zero fill is why a truncated group still lands on a real
        // alphabet character; the padding below is what records how much of it
        // was real.
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        let sextet = |shift: u32| ALPHABET[(n >> shift) as usize & 63] as char;
        out.push(sextet(18));
        out.push(sextet(12));
        // One `=` for a two-byte tail, two for a one-byte tail: the padding says
        // how many whole bytes the last group carried, so a decoder recovers the
        // exact length instead of inventing a trailing zero.
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

/// Extract embedded raster images living under `dir_prefix` (e.g. `word/media/`
/// for docx, `ppt/media/` for pptx) from an OOXML zip into a per-process temp
/// directory, returning the written paths sorted by archive name.
pub fn extract_ooxml_media(archive: &str, dir_prefix: &str) -> Vec<PathBuf> {
    extract_zip_images(archive, |n| n.starts_with(dir_prefix) && is_raster_name(n))
}

/// Extract embedded raster images from an epub zip. Unlike OOXML, epub images
/// live under no single fixed prefix (a book may scatter them across `images/`,
/// `OEBPS/img/`, …), so every raster member anywhere in the archive is kept.
pub fn extract_epub_media(archive: &str) -> Vec<PathBuf> {
    extract_zip_images(archive, is_raster_name)
}

/// Shared core for the format-specific media extractors: write every zip member
/// whose name satisfies `keep` into a per-process temp directory, returning the
/// written paths sorted by archive name. Best-effort: any error (not a zip,
/// unreadable member, write failure) is skipped, and an archive with no matching
/// media yields an empty vec. The temp files live for the viewer's lifetime; they
/// are small and bounded by the document's own media. The archive-relative name
/// is flattened (`/` → `_`) into the temp filename so members that share a
/// basename across directories (common in epub) can't collide and overwrite.
fn extract_zip_images(archive: &str, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(file) = std::fs::File::open(archive) else {
        return Vec::new();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| keep(n))
        .collect();
    names.sort();
    if names.is_empty() {
        return Vec::new();
    }
    let dir = std::env::temp_dir().join(format!("sucher-media-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in names {
        let Ok(mut f) = zip.by_name(&name) else {
            continue;
        };
        let mut bytes = Vec::new();
        if f.read_to_end(&mut bytes).is_err() {
            continue;
        }
        let dest = dir.join(name.replace('/', "_"));
        if std::fs::write(&dest, &bytes).is_ok() {
            out.push(dest);
        }
    }
    out
}

/// Does this archive member name end in a raster image extension the `image`
/// crate can decode? (SVG/EMF/WMF vector media are skipped — no in-tree decoder.)
fn is_raster_name(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".tiff", ".tif", ".webp",
    ]
    .iter()
    .any(|e| n.ends_with(e))
}

/// Decode an XML `Text` event's bytes to a string. quick-xml (≥ 0.37) emits
/// entity references as separate [`Event::GeneralRef`](quick_xml::events::Event)
/// events, so a `Text` event never contains `&…;` — decoding is all that's
/// needed here; see [`xml_ref`] for the entity side. Empty on a decode error.
pub fn xml_text(t: &BytesText) -> String {
    t.as_ref().to_owned()
}

/// Resolve an XML entity reference (`Event::GeneralRef`) to its text: the five
/// predefined entities (`amp`/`lt`/`gt`/`quot`/`apos`) and numeric char refs
/// (`#65`, `#x41`). We rebuild the `&name;` form and reuse quick-xml's own
/// unescaper so the mapping stays authoritative. Unknown entities → empty.
pub fn xml_ref(r: &BytesRef) -> String {
    let name = r.as_ref();
    quick_xml::escape::unescape(&format!("&{name};"))
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

/// Human-readable byte size (e.g. `1.2K`, `340 B`).
pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < 4 {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1}{}", U[i])
}

/// Compact relative age of `t` as of `now`, a single unit ≤4 chars for the
/// browser's "modified" column: `now`/`12s`/`5m`/`3h`/`2d`/`6w`/`4mo`/`3y`.
///
/// `now` is a parameter so the whole mapping is pure and unit-testable without
/// reading the clock. A `t` in the future (clock skew, a file stamped ahead)
/// clamps to `now` rather than underflowing. Thresholds are chosen so each unit
/// stays legible: seconds under a minute, minutes under an hour, hours under a
/// day, days under a week, weeks under ~two months, months under a year, then
/// years.
pub fn human_age(t: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;
    match secs {
        s if s < 5 => "now".into(),
        s if s < MIN => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < WEEK => format!("{}d", s / DAY),
        s if s < 2 * MONTH => format!("{}w", s / WEEK),
        s if s < YEAR => format!("{}mo", s / MONTH),
        s => format!("{}y", s / YEAR),
    }
}

/// Coarse relative time since `t` (e.g. `just now`, `3d ago`).
pub fn rel_time(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 86_400 * 30 => format!("{}d ago", s / 86_400),
        s if s < 86_400 * 365 => format!("{}mo ago", s / (86_400 * 30)),
        s => format!("{}y ago", s / (86_400 * 365)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1024 * 1024), "1.0M");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0G");
    }

    #[test]
    fn rel_time_buckets() {
        let now = SystemTime::now();
        assert_eq!(rel_time(now), "just now");
        assert_eq!(rel_time(now - Duration::from_secs(120)), "2m ago");
        assert_eq!(rel_time(now - Duration::from_secs(3 * 86_400)), "3d ago");
    }

    #[test]
    fn human_age_across_the_ranges() {
        let now = SystemTime::now();
        let ago = |d: Duration| now - d;
        // Under 5s (and exactly now) reads "now".
        assert_eq!(human_age(now, now), "now");
        assert_eq!(human_age(ago(Duration::from_secs(3)), now), "now");
        // Seconds, minutes, hours.
        assert_eq!(human_age(ago(Duration::from_secs(30)), now), "30s");
        assert_eq!(human_age(ago(Duration::from_secs(5 * 60)), now), "5m");
        assert_eq!(human_age(ago(Duration::from_secs(3 * 3600)), now), "3h");
        // Days, weeks (6w = 42d must be reachable), months, years.
        assert_eq!(human_age(ago(Duration::from_secs(2 * 86_400)), now), "2d");
        assert_eq!(human_age(ago(Duration::from_secs(42 * 86_400)), now), "6w");
        assert_eq!(
            human_age(ago(Duration::from_secs(120 * 86_400)), now),
            "4mo"
        );
        assert_eq!(
            human_age(ago(Duration::from_secs(3 * 365 * 86_400)), now),
            "3y"
        );
        // A timestamp in the future clamps to "now" rather than underflowing.
        assert_eq!(human_age(now + Duration::from_secs(60), now), "now");
    }

    #[test]
    fn capped_reads_under_and_at_the_limit() {
        let data = [b'a'; 10];
        // Exactly at the cap is accepted (we read max+1 and only reject on >max).
        assert_eq!(read_to_string_capped(&data[..], 10).unwrap().len(), 10);
        // Comfortably under the cap is accepted.
        assert_eq!(read_to_string_capped(&data[..], 20).unwrap().len(), 10);
    }

    #[test]
    fn capped_rejects_one_byte_over_the_limit() {
        let data = [b'a'; 11];
        assert!(read_to_string_capped(&data[..], 10).is_err());
    }

    #[test]
    fn capped_rejects_invalid_utf8() {
        let data = [0xff, 0xfe, 0x00];
        assert!(read_to_string_capped(&data[..], 100).is_err());
    }

    #[test]
    fn capped_stops_a_bomb_without_unbounded_allocation() {
        // A reader that would yield ~1 TiB if drained. `read_to_string_capped`
        // must `take` it to max+1 first, so at most max+1 bytes are ever
        // allocated before it detects the overflow and returns Err — never a
        // truncated string and never an OOM.
        let bomb = std::io::repeat(b'a').take(1 << 40);
        let max = 1024;
        assert!(
            read_to_string_capped(bomb, max).is_err(),
            "a source larger than the cap must be rejected, not truncated"
        );
    }

    #[test]
    fn cmd_path_arg_guards_leading_dash() {
        // Absolute paths pass through untouched.
        assert_eq!(cmd_path_arg("/abs/x.pdf"), "/abs/x.pdf");
        // A relative path that would parse as an option gets a `./` prefix.
        assert_eq!(cmd_path_arg("-x.pdf"), "./-x.pdf");
        // An ordinary relative path is anchored too.
        assert_eq!(cmd_path_arg("sub/f.pdf"), "./sub/f.pdf");
        // An already-anchored path is left as-is (no redundant `././`).
        assert_eq!(cmd_path_arg("./already"), "./already");
        // A bare `-` cannot slip through as an option.
        assert_eq!(cmd_path_arg("-"), "./-");
    }

    #[test]
    fn is_safe_url_allow_list() {
        assert!(is_safe_url("http://example.com"));
        assert!(is_safe_url("https://example.com/a?b=c"));
        assert!(is_safe_url("mailto:a@b.com"));
        // Scheme match is case-insensitive.
        assert!(is_safe_url("HTTPS://Example.com"));
        // Everything else is refused.
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("custom:whatever"));
        assert!(!is_safe_url("-x"));
        assert!(!is_safe_url(""));
    }

    #[test]
    fn run_with_timeout_captures_output_of_fast_command() {
        let mut cmd = Command::new("printf");
        cmd.arg("hello");
        let out = run_with_timeout(cmd, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn run_with_timeout_kills_a_hang() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = run_with_timeout(cmd, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
    }

    #[test]
    fn base64_encodes_the_rfc4648_vectors() {
        // The canonical progression from RFC 4648 section 10: it walks every
        // tail length twice over, so a shift or an off-by-one in the packing
        // shows up here before it reaches a clipboard.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"Man"), "TWFu");
    }

    #[test]
    fn base64_pads_by_input_length_mod_three() {
        // len % 3 == 0: no padding, and the output is exactly 4/3 the input.
        assert_eq!(base64_encode(b"abcdef"), "YWJjZGVm");
        // len % 3 == 2: one `=`.
        assert_eq!(base64_encode(b"abcde"), "YWJjZGU=");
        // len % 3 == 1: two `=`.
        assert_eq!(base64_encode(b"abcd"), "YWJjZA==");
        // In every case the length is a multiple of four, which is what a
        // terminal's decoder expects to receive.
        for n in 0..32usize {
            let input = vec![b'x'; n];
            assert_eq!(base64_encode(&input).len() % 4, 0, "n = {n}");
        }
    }

    #[test]
    fn base64_encodes_bytes_not_chars() {
        // Bytes above 0x7F must go through as bytes. `ä` is U+00E4, two UTF-8
        // bytes (C3 A4); encoding the *char* would produce something else
        // entirely, so this pins the byte-oriented reading.
        assert_eq!(base64_encode("ä".as_bytes()), "w6Q=");
        assert_eq!(base64_encode("€".as_bytes()), "4oKs");
        // The top of the alphabet (`+` at 62, `/` at 63) is only reachable with
        // high bytes, so this also proves both are emitted.
        assert_eq!(base64_encode(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(base64_encode(&[0xfb, 0xef, 0xbe]), "++++");
        assert_eq!(base64_encode(&[0x00, 0x00, 0x00]), "AAAA");
    }

    #[test]
    fn osc52_sequence_frames_the_payload_exactly() {
        // Introducer, the `c` (clipboard) selection, the base64, then BEL.
        assert_eq!(osc52_sequence("hi").unwrap(), b"\x1b]52;c;aGk=\x07");
        assert_eq!(
            osc52_sequence("/tmp/a.pdf").unwrap(),
            b"\x1b]52;c;L3RtcC9hLnBkZg==\x07"
        );
        // Empty text is legal: it is the sequence that clears the clipboard, and
        // refusing it here would be a rule the terminal does not have.
        assert_eq!(osc52_sequence("").unwrap(), b"\x1b]52;c;\x07");
    }

    #[test]
    fn osc52_sequence_cannot_be_broken_out_of() {
        // A filename may legally contain an ESC, a BEL, or a whole forged OSC
        // command. Base64 renders all of it as alphabet characters, so the
        // finished sequence holds exactly one ESC (the introducer) and one BEL
        // (the terminator), both at the ends.
        let hostile = "\x1b]52;c;ZXZpbA==\x07\x1b[2J";
        let seq = osc52_sequence(hostile).unwrap();
        assert_eq!(seq.iter().filter(|&&b| b == 0x1b).count(), 1);
        assert_eq!(seq.iter().filter(|&&b| b == 0x07).count(), 1);
        assert_eq!(seq[0], 0x1b);
        assert_eq!(seq[seq.len() - 1], 0x07);
        assert!(seq[7..seq.len() - 1]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/' || *b == b'='));
    }

    #[test]
    fn osc52_sequence_refuses_rather_than_truncates_past_the_cap() {
        // Exactly at the cap is accepted, so the documented number is the number.
        let at_cap = "a".repeat(MAX_CLIPBOARD_BYTES);
        assert!(osc52_sequence(&at_cap).is_ok());
        // One byte over is refused outright, and the error names the size, so
        // the caller can say what happened instead of silently yanking a prefix.
        let over = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
        let err = osc52_sequence(&over).unwrap_err();
        assert!(err.contains("64.0K"), "error should name the cap: {err}");
    }

    #[test]
    fn osc52_cap_counts_bytes_so_multibyte_text_cannot_slip_past() {
        // `text.len()` is bytes, not chars: a string of 2-byte characters that
        // is only half the cap in `chars()` is still over it in bytes, which is
        // the measure the terminal's limit is expressed in.
        let multibyte = "ä".repeat(MAX_CLIPBOARD_BYTES / 2 + 1);
        assert!(multibyte.chars().count() < MAX_CLIPBOARD_BYTES);
        assert!(osc52_sequence(&multibyte).is_err());
    }

    #[test]
    fn extracts_embedded_images_from_samples() {
        let d = extract_ooxml_media("samples/sample.docx", "word/media/");
        assert_eq!(d.len(), 1, "docx should have 1 image");
        let p = extract_ooxml_media("samples/deck.pptx", "ppt/media/");
        assert_eq!(p.len(), 2, "pptx should have 2 images");
        assert!(p.iter().all(|x| x.exists()), "extracted files exist");
    }
}
