# 0013 — A blocking workbook load for the non-interactive path

Status: proposed (decided; implementation is follow-up work)
Date: 2026-07-09

## Context

The spreadsheet grid streams rows on a background thread (`xlsx::StreamBook`), so
the interactive viewer opens instantly on a huge workbook and stays responsive
while rows load — a good design for the *interactive* case.

But the non-interactive `sheet::dump` (pipe mode) does not want streaming at all:
it needs every row before it can print. It fakes synchronous behaviour by leaking
the streaming machinery (`sheet.rs:312-341`):

```rust
loop {
    if b.dims().2 { break; }             // poll the "done" flag
    std::thread::sleep(Duration::from_millis(50));   // busy-wait
}
```

and it opens the workbook once *per sheet* inside the loop, paying a full
re-parse for each worksheet. This is an abstraction mismatch — a streaming reader
used where a blocking one is wanted — expressed as a sleep-poll and redundant
re-opens. (ADR 0009's `xlsx::preview_rows` already showed the parser can be driven
synchronously; this ADR extends that to a full blocking load.)

## Decision

Give the workbook a blocking load and use it in the dump path instead of
sleep-polling the streaming loader:

- Add `StreamBook::load_all(path) -> Result<LoadedBook, String>` (or a
  `wait_ready(&self)` that blocks on the worker's join handle) that parses the
  whole workbook synchronously — reusing the shared `parse_sheet_xml` from ADR
  0009 so there is still one parser — bounded by the existing `ROW_CAP`.
- `sheet::dump` opens once, iterates sheets over the loaded book, and drops the
  `sleep`/`dims().2` poll and the per-sheet re-open entirely.
- The interactive viewer keeps the streaming `StreamBook` — that is where
  streaming earns its keep. The two share the parser, not the concurrency model.

## Consequences

- The dump path is deterministic and has no busy-wait; a large workbook is parsed
  once, not re-parsed per sheet.
- Streaming stays an interactive-only concern; the "wait for streaming to finish"
  contortion disappears from the batch path.
- The row/cell parser remains single-sourced (`parse_sheet_xml`), so streaming,
  preview (ADR 0009), and the blocking load can never diverge.
- Small, self-contained; independent of ADRs 0010–0012. Low risk, clear win.
