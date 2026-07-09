# 0009 — Bounded decoders (untrusted-input resource limits)

Status: accepted
Date: 2026-07-09

## Context

sucher opens and *previews* files it did not create. The directory browser makes
this sharper than a normal viewer: moving the selection onto a file
**synchronously parses it** to build the preview (`dir.rs::build_preview`), so
merely scrolling past a file invokes its full decoder. A security review found
that several decoders read their entire input into memory with no bound:

- `html::to_markdown` — `read_to_string` of the whole file, then a full DOM.
- `docx`/`pptx::to_markdown` — `read_to_string` of a decompressed zip member.
- the spreadsheet preview — calamine `worksheet_range`, which materialises the
  entire sheet (bypassing the streaming row cap the interactive grid uses).
- csv/tsv preview — `fs::read` of the whole file.
- archive listing — a `.tar.gz`/`.gz` is fully inflated just to enumerate names
  (a bare `.gz` is even inflated speculatively to test whether it is a tar).
- image decode — no pixel-dimension limit (only `image`'s 512 MiB default).
- poppler/ffmpeg subprocesses — no timeout; a hung tool wedges every later
  preview forever.

A crafted file (a zip/gzip bomb whose few KB inflate to gigabytes, or a plain
multi-GB HTML/CSV, or an image claiming enormous dimensions) turns "scroll onto
it" into an OOM kill or an unbounded hang, with no click required. On the remote
S3/GCS mounts the project supports it also turns into unbounded network reads.

## Decision

**Every decoder is responsible for bounding its own resource use.** The limit
lives in the decoder, not at each call site, so *both* the browser preview and an
explicit interactive open inherit it — the threat is the same file either way.

Concretely:

1. **Decompressed/read byte cap.** Each decoder reads its input through a
   byte-limited reader that stops at a documented hard cap
   (`MAX_DECODE_BYTES`) and returns an honest `Err` ("file too large to
   preview/parse") past it — never a silent truncation that would feed malformed
   half-markup to a parser. This bounds *memory* (a bomb inflates at most the cap
   before we stop) and *time* (parse work is proportional to the cap). Applies to
   html, docx, pptx, and archive inflation. The cap is generous enough for real
   documents and small enough that a bounded parse is sub-second.
2. **Spreadsheets use the streaming reader, not full materialisation.** The
   preview path uses the existing `StreamBook` + row cap rather than calamine's
   `worksheet_range`, so a huge sheet is bounded by the same row cap the
   interactive grid already relies on. csv/tsv reads are byte-capped like the rest.
3. **Images set explicit pixel limits.** Every `image::ImageReader` sets
   `Limits` with a `max_image_width`/`max_image_height` tied to a sane multiple of
   the display, so a small file claiming enormous dimensions cannot force a huge
   allocation on decode.
4. **Subprocess decoders get a wall-clock timeout + kill.** poppler/ffmpeg
   invocations are bounded by a timeout; on expiry the child is killed and reaped
   and the in-flight raster slot is cleared, so one hung tool cannot starve all
   later previews.

## Why not move the heavy parsers to a background thread?

The perf review noted the synchronous parse on the UI thread. We deliberately do
**not** move text/document previews onto the async raster worker:

- ADR 0005 made a considered split — only *slow* raster (image/PDF/video, 100s of
  ms) went async behind a "Loading…" placeholder; text previews stayed
  synchronous because they are fast. Bounding the input (above) keeps that true:
  a capped parse is milliseconds, well within a frame.
- The freeze the review saw was caused by *unboundedness*, not by being on the UI
  thread. Removing the unboundedness removes the freeze at its root.
- An async text path would add a "Loading…" flicker to previews that currently
  appear instantly, and would duplicate the worker's install/coalesce machinery —
  a worse experience and more surface for a sub-frame benefit.
- `build_preview` results are cached (`preview_for`), so even a near-cap file
  parses at most once as you land on it, not per frame.

If a class of legitimately-large files ever makes the one-shot parse feel slow,
moving *that* class async is a future ADR — but it is not warranted to defend
against untrusted input, which caps handle correctly.

## Consequences

- A single, uniform principle ("the decoder bounds itself") closes the
  decompression-bomb, unbounded-read, huge-image, and hung-subprocess vectors for
  both preview and open.
- Files beyond a cap show an honest "too large" line instead of hanging — a
  visible, correct degradation.
- The byte-limited reader and the caps are pure/small and unit-tested.
- No new async surface; the ADR 0005 sync-text / async-raster split stands.
