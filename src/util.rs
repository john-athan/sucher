// Small shared formatting helpers used across viewers (the directory browser's
// list/preview, and the "no viewer" metadata line). Kept here so there is one
// implementation rather than per-module copies that could drift.

use quick_xml::events::{BytesRef, BytesText};
use std::io::{self, ErrorKind, Read};
use std::path::PathBuf;
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

/// Extract embedded raster images living under `dir_prefix` (e.g. `word/media/`
/// for docx, `ppt/media/` for pptx) from an OOXML zip into a per-process temp
/// directory, returning the written paths sorted by archive name. Best-effort:
/// any error (not a zip, unreadable member, write failure) is skipped, and an
/// archive with no media yields an empty vec. The temp files live for the
/// viewer's lifetime; they are small and bounded by the document's own media.
pub fn extract_ooxml_media(archive: &str, dir_prefix: &str) -> Vec<PathBuf> {
    let Ok(file) = std::fs::File::open(archive) else {
        return Vec::new();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| n.starts_with(dir_prefix) && is_raster_name(n))
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
        let file_name = std::path::Path::new(&name)
            .file_name()
            .map(|s| s.to_owned())
            .unwrap_or_default();
        let dest = dir.join(file_name);
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
    t.decode().map(|c| c.into_owned()).unwrap_or_default()
}

/// Resolve an XML entity reference (`Event::GeneralRef`) to its text: the five
/// predefined entities (`amp`/`lt`/`gt`/`quot`/`apos`) and numeric char refs
/// (`#65`, `#x41`). We rebuild the `&name;` form and reuse quick-xml's own
/// unescaper so the mapping stays authoritative. Unknown entities → empty.
pub fn xml_ref(r: &BytesRef) -> String {
    let name = r.decode().map(|c| c.into_owned()).unwrap_or_default();
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
    fn extracts_embedded_images_from_samples() {
        let d = extract_ooxml_media("samples/sample.docx", "word/media/");
        assert_eq!(d.len(), 1, "docx should have 1 image");
        let p = extract_ooxml_media("samples/deck.pptx", "ppt/media/");
        assert_eq!(p.len(), 2, "pptx should have 2 images");
        assert!(p.iter().all(|x| x.exists()), "extracted files exist");
    }
}
