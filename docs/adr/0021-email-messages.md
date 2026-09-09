# ADR 0021: Saved email messages, two containers, one viewer

Status: **Accepted, 2026-09-09**

## Context

A saved message is one of the files this tool exists for: it arrives as an
attachment or an export, no browser opens it, and double-clicking it either
launches a mail client that wants to import the thing or shows nothing at all.
Before this change `s message.eml` fell through to `Format::Text` (the raw
source: base64 blobs, quoted-printable, MIME boundaries) and `s message.msg`
classified as `Format::Binary` and opened in the hex viewer.

Two containers carry the same thing:

- **`.eml`**, RFC 5322 with MIME (RFC 2045 to 2049): headers, nested
  multiparts, `base64` and `quoted-printable` transfer encodings, RFC 2047
  encoded words in headers, and a charset per part.
- **`.msg`**, Outlook's export: a Compound File Binary container (the OLE2
  format, the same family as the legacy `.doc`) holding MAPI properties in
  streams named by property tag, with one sub-storage per attachment.

Neither is a few lines of parsing. MIME alone is boundary tracking, two
transfer encodings, encoded words, and 40-odd charsets, all of it exposed to
whatever a stranger sent.

## Decision

**D1: A message reduces to markdown, like every other document container.**
`src/email.rs` fills one `Mail` struct (subject, from, to, cc, date, body,
attachment list) and renders it as markdown, so the existing markdown TUI shows
it. No new viewer, no new key bindings, and the piped `--plain` dump, the
browser's preview pane, and recursive search all inherit it for free. This is
the docx/pptx/epub/html pattern (ADR 0008, ADR 0010), applied again.

**D2: One `Format::Email` for both extensions; the bytes pick the reader.**
`.eml` and `.msg` classify identically, and `email::to_markdown` decides which
container it holds by trying to open it as a CFB: what `cfb` accepts is a
`.msg`, everything else is parsed as RFC 5322. A misnamed file therefore still
opens correctly, and the classifier stays a pure function of the file name
(ADR 0001) rather than growing a second sniffing path.

**D3: MIME is a dependency, not a hand-rolled parser.** `mail-parser`
(Apache-2.0 OR MIT, from the Stalwart mail server) is a fuzzed, MIRI-tested,
dependency-light implementation of the RFCs above, liberal in what it accepts,
which is the only workable posture for real mail. Writing base64,
quoted-printable, encoded-word and charset decoding here would be a few hundred
lines of the kind of code that is wrong at exactly the edges we would not
notice. `full_encoding` is enabled so a legacy multi-byte charset decodes
rather than mojibakes.

**D4: `.msg` is read directly off the property streams with `cfb`** (MIT). The
fields a reader wants are seven streams: `__substg1.0_0037001F` is the subject,
`0C1A`/`0C1F` the sender name and address, `0E04`/`0E03` the display To and Cc,
`1013` the HTML body with `1000` the plain fallback, and each attachment is a
`__attach_version1.0_#…` sub-storage with its name in `3707` and its bytes in
`3701`. That is a lookup table, not a parser, so it lives here rather than in a
third-party `.msg` crate. A `.msg` stores no parsed date, only the raw
transport headers when it has them at all, so the date comes from feeding
property `007D` to the RFC 5322 date parser we already link. Property strings
come as UTF-16LE (`001F`) or as 8-bit (`001E`) whose codepage is named by yet
another property; the 8-bit case is decoded as UTF-8, then Latin-1, which
covers Outlook 2003 and later plus western European legacy files.

**D5: An HTML body goes through the shared HTML reducer.** Most mail that
matters is HTML, and `html::markdown_from_str` (ADR 0008) already turns it into
the markdown vocabulary this app renders. A plain-text body is passed through
as written, so its `>` quoting arrives as blockquotes.

**D6: Attachments are listed, raster ones are shown.** The rendered document
ends with a name and size per attachment, and every attachment the `image`
crate can decode is extracted to the per-process temp directory that already
feeds the viewer's image gallery, so an inline chart is visible rather than
merely named. Extraction is bounded like every other decoder (ADR 0009): the
whole `.eml` read is capped at `MAX_DECODE_BYTES`, as is each `.msg` stream.

## Consequences

- `s message.eml` and `s message.msg` open the message: subject as the heading,
  envelope, body, attachment list, inline images in the gallery. Both preview
  in the browser and dump under `--plain`.
- `kind:email` (also `mail`, `eml`, `msg`) filters for them in the browser and
  in recursive search, and `content:` grep still reads the raw file, so a
  search hit inside a base64 attachment is possible; that is the same honest
  behaviour every binary container has here.
- Two dependencies enter the tree, both permissively licensed and both small.
  `mail-parser` brings `hashify`; `cfb` brings `web-time`.
- What is deliberately not done: no thread view, no `.mbox` or `.emlx`, no
  reply/forward, no S/MIME or PGP decryption. A signed message shows its parts;
  an encrypted one shows that it is encrypted. This is a viewfinder, not a mail
  client (see the scope section of the README).
