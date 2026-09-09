// Email messages -> markdown (ADR 0021). Two containers, one output:
//
//   .eml  RFC 5322 / MIME, parsed with `mail-parser` (multipart, base64 and
//         quoted-printable transfer encodings, RFC 2047 encoded words, 40+
//         charsets). The HTML body, when the message has one, goes through the
//         shared HTML reducer (`html::markdown_from_str`), so a marketing mail
//         reads as text rather than as tag soup.
//   .msg  Outlook's Compound File Binary property store, read with `cfb`. The
//         fields a reader wants live in root streams named by property tag
//         (`__substg1.0_0037001F` is the subject), attachments in
//         `__attach_version1.0_#…` sub-storages.
//
// Both fill the same [`Mail`] struct and share one markdown formatter, so the
// existing markdown TUI renders either, no new UI (mirrors docx/pptx/epub).
// Which container a file is comes from the bytes, not the extension: `.msg` is
// whatever `cfb` can open, everything else is parsed as RFC 5322.

use mail_parser::{Address, Message, MessageParser, MimeHeaders};
use std::io::{Read, Seek};
use std::path::PathBuf;

/// A message reduced to the fields a reader wants, container-independent. The
/// .eml and .msg readers each fill one in; [`Mail::render`] is the single
/// markdown formatter they share.
#[derive(Default, Debug, PartialEq)]
struct Mail {
    subject: String,
    from: String,
    to: String,
    cc: String,
    date: String,
    /// The body, already markdown (an HTML body is reduced by `html`).
    body: String,
    /// (file name, byte size) per attachment. Listed, not inlined; raster
    /// attachments additionally reach the viewer's gallery through [`media`].
    attachments: Vec<(String, u64)>,
}

impl Mail {
    /// The markdown a viewer renders: a heading with the subject, the envelope
    /// as hard-broken lines (a soft break would fold them into one paragraph in
    /// the piped dump), a rule, the body, and the attachment list.
    fn render(&self) -> String {
        let subject = self.subject.trim();
        let mut out = format!(
            "# {}\n\n",
            if subject.is_empty() {
                "(no subject)"
            } else {
                subject
            }
        );
        for (label, value) in [
            ("From", &self.from),
            ("To", &self.to),
            ("Cc", &self.cc),
            ("Date", &self.date),
        ] {
            let value = value.trim();
            if !value.is_empty() {
                out.push_str(&format!("**{label}:** {value}  \n"));
            }
        }
        out.push_str("\n---\n\n");
        let body = self.body.trim();
        out.push_str(if body.is_empty() { "*(no body)*" } else { body });
        out.push('\n');
        if !self.attachments.is_empty() {
            out.push_str("\n---\n\n**Attachments**\n\n");
            for (name, size) in &self.attachments {
                out.push_str(&format!(
                    "- {name}  ·  {}\n",
                    crate::util::human_size(*size)
                ));
            }
        }
        out
    }
}

pub fn to_markdown(path: &str) -> Result<String, String> {
    match cfb::open(path) {
        Ok(comp) => Ok(msg_mail(comp)?.render()),
        Err(_) => Ok(eml_mail(&read_message(path)?)?.render()),
    }
}

/// Raster images carried by a message, extracted to temp files for the viewer's
/// image gallery (the same treatment docx/epub media gets). Empty when the
/// message has none, or on any read error.
pub fn media(path: &str) -> Vec<PathBuf> {
    let items = match cfb::open(path) {
        Ok(comp) => msg_media(comp),
        Err(_) => read_message(path)
            .ok()
            .map(|raw| eml_media(&raw))
            .unwrap_or_default(),
    };
    crate::util::write_media(&items)
}

/// Read a whole message file, bounded (ADR 0009). An `.eml` is parsed from
/// memory, so a multi-GB file must not be read whole; past the cap we report
/// honestly instead.
fn read_message(path: &str) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    crate::util::read_capped(file, crate::util::MAX_DECODE_BYTES)
}

// ---------------------------------------------------------------- .eml (MIME)

fn eml_mail(raw: &[u8]) -> Result<Mail, String> {
    let m = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| "not a readable email message".to_string())?;
    Ok(mail_from_message(&m))
}

/// PURE, unit-tested: the whole .eml path minus the file read.
fn mail_from_message(m: &Message) -> Mail {
    Mail {
        subject: m.subject().unwrap_or_default().to_string(),
        from: addresses(m.from()),
        to: addresses(m.to()),
        cc: addresses(m.cc()),
        // RFC 3339 rather than the raw header: unambiguous, and it costs no
        // date-formatting dependency.
        date: m.date().map(|d| d.to_rfc3339()).unwrap_or_default(),
        body: body_markdown(m),
        attachments: m
            .attachments()
            .filter(|p| !p.is_multipart())
            .map(|p| {
                let name = p.attachment_name().unwrap_or("(unnamed)").to_string();
                (name, p.len() as u64)
            })
            .collect(),
    }
}

/// The body as markdown: a real HTML part is reduced by the shared HTML
/// reducer, otherwise the plain-text part is taken as written (its `>` quoting
/// then renders as blockquotes). `body_html` synthesises HTML from a text-only
/// message, so the choice is made on the part's own type, not on its presence.
fn body_markdown(m: &Message) -> String {
    if m.html_part(0).is_some_and(|p| p.is_text_html()) {
        if let Some(html) = m.body_html(0) {
            return crate::html::markdown_from_str(&html);
        }
    }
    m.body_text(0).unwrap_or_default().into_owned()
}

/// One header's addresses as `Name <addr>, …`. Groups are flattened; an address
/// with no display name shows bare.
fn addresses(a: Option<&Address>) -> String {
    let Some(a) = a else {
        return String::new();
    };
    a.iter()
        .map(|addr| {
            match (
                addr.name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                addr.address.as_deref(),
            ) {
                (Some(n), Some(e)) => format!("{n} <{e}>"),
                (Some(n), None) => n.to_string(),
                (None, Some(e)) => e.to_string(),
                (None, None) => String::new(),
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Raster parts of an .eml as (name, bytes) for [`crate::util::write_media`].
/// A part with no file name is named after its subtype (`image/png` →
/// `part3.png`) so the extension gate still recognises it.
fn eml_media(raw: &[u8]) -> Vec<(String, Vec<u8>)> {
    let Some(m) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    m.attachments()
        .enumerate()
        .filter(|(_, p)| p.is_binary())
        .map(|(i, p)| {
            let name = p.attachment_name().map(str::to_string).unwrap_or_else(|| {
                let sub = p
                    .content_type()
                    .and_then(|c| c.subtype())
                    .unwrap_or("bin")
                    .to_ascii_lowercase();
                format!("part{i}.{sub}")
            });
            (name, p.contents().to_vec())
        })
        .filter(|(name, _)| crate::util::is_raster_name(name))
        .collect()
}

// ------------------------------------------------------------- .msg (Outlook)

/// MAPI property ids for the fields we show. The stream holding one is named
/// `__substg1.0_<id><type>`, e.g. subject as Unicode is `__substg1.0_0037001F`.
const P_SUBJECT: u16 = 0x0037;
const P_SENDER_NAME: u16 = 0x0C1A;
const P_SENDER_EMAIL: u16 = 0x0C1F;
const P_DISPLAY_TO: u16 = 0x0E04;
const P_DISPLAY_CC: u16 = 0x0E03;
const P_BODY: u16 = 0x1000;
const P_BODY_HTML: u16 = 0x1013;
const P_TRANSPORT_HEADERS: u16 = 0x007D;
const P_ATTACH_LONG_NAME: u16 = 0x3707;
const P_ATTACH_NAME: u16 = 0x3704;
const P_ATTACH_DATA: u16 = 0x3701;

fn msg_mail<F: Read + Seek>(mut c: cfb::CompoundFile<F>) -> Result<Mail, String> {
    let from = match (
        prop(&mut c, "/", P_SENDER_NAME),
        prop(&mut c, "/", P_SENDER_EMAIL),
    ) {
        (Some(n), Some(e)) if !n.trim().is_empty() => format!("{} <{}>", n.trim(), e.trim()),
        (n, e) => n.or(e).unwrap_or_default(),
    };
    // A .msg keeps no parsed date, only the raw transport headers (when it has
    // them at all, sent items often don't). Reuse the RFC 5322 date parser we
    // already link rather than growing a second one here.
    let date = prop(&mut c, "/", P_TRANSPORT_HEADERS)
        .and_then(|h| {
            MessageParser::default()
                .parse(h.as_bytes())?
                .date()
                .cloned()
        })
        .map(|d| d.to_rfc3339())
        .unwrap_or_default();
    let body = match prop(&mut c, "/", P_BODY_HTML) {
        Some(html) if !html.trim().is_empty() => crate::html::markdown_from_str(&html),
        _ => prop(&mut c, "/", P_BODY).unwrap_or_default(),
    };
    let attachments = attach_dirs(&c)
        .into_iter()
        .map(|dir| {
            let name = prop(&mut c, &dir, P_ATTACH_LONG_NAME)
                .or_else(|| prop(&mut c, &dir, P_ATTACH_NAME))
                .unwrap_or_else(|| "(unnamed)".to_string());
            let size = c
                .entry(format!("{dir}{}", stream_name(P_ATTACH_DATA, "0102")))
                .map(|e| e.len())
                .unwrap_or(0);
            (name, size)
        })
        .collect();
    Ok(Mail {
        subject: prop(&mut c, "/", P_SUBJECT).unwrap_or_default(),
        from,
        to: prop(&mut c, "/", P_DISPLAY_TO).unwrap_or_default(),
        cc: prop(&mut c, "/", P_DISPLAY_CC).unwrap_or_default(),
        date,
        body,
        attachments,
    })
}

/// Raster attachments of a .msg as (name, bytes) for [`crate::util::write_media`].
fn msg_media<F: Read + Seek>(mut c: cfb::CompoundFile<F>) -> Vec<(String, Vec<u8>)> {
    attach_dirs(&c)
        .into_iter()
        .filter_map(|dir| {
            let name = prop(&mut c, &dir, P_ATTACH_LONG_NAME)
                .or_else(|| prop(&mut c, &dir, P_ATTACH_NAME))?;
            if !crate::util::is_raster_name(&name) {
                return None;
            }
            let data = stream(
                &mut c,
                &format!("{dir}{}", stream_name(P_ATTACH_DATA, "0102")),
            )?;
            Some((name, data))
        })
        .collect()
}

/// The `/__attach_version1.0_#…/` sub-storages, one per attachment, in
/// directory order.
fn attach_dirs<F>(c: &cfb::CompoundFile<F>) -> Vec<String> {
    c.read_root_storage()
        .filter(|e| e.is_storage() && e.name().starts_with("__attach"))
        .map(|e| format!("/{}/", e.name()))
        .collect()
}

/// A property stream's name for an id and MAPI type, e.g. `(0x0037, "001F")`
/// → `__substg1.0_0037001F`.
fn stream_name(id: u16, ty: &str) -> String {
    format!("__substg1.0_{id:04X}{ty}")
}

/// A string property from a storage, trying each type the field is written as:
/// Unicode (UTF-16LE), 8-bit, then binary (PR_HTML is stored that way). None
/// when the storage carries the property in no form.
fn prop<F: Read + Seek>(c: &mut cfb::CompoundFile<F>, dir: &str, id: u16) -> Option<String> {
    if let Some(b) = stream(c, &format!("{dir}{}", stream_name(id, "001F"))) {
        return Some(utf16le(&b));
    }
    for ty in ["001E", "0102"] {
        if let Some(b) = stream(c, &format!("{dir}{}", stream_name(id, ty))) {
            return Some(eight_bit(b));
        }
    }
    None
}

/// A stream's bytes, capped like every other decoder (ADR 0009). None when the
/// stream is absent or unreadable.
fn stream<F: Read + Seek>(c: &mut cfb::CompoundFile<F>, path: &str) -> Option<Vec<u8>> {
    let s = c.open_stream(path).ok()?;
    let mut buf = Vec::new();
    s.take(crate::util::MAX_DECODE_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

/// Decode a PT_UNICODE property (UTF-16LE, no BOM). A trailing odd byte is
/// dropped; unpaired surrogates become U+FFFD.
fn utf16le(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Decode a PT_STRING8 / PT_BINARY property. The bytes carry no charset of
/// their own (PR_INTERNET_CPID names it, in yet another property), so: UTF-8
/// when it parses, which covers everything Outlook 2003 and later writes, else
/// Latin-1, which keeps western European text readable.
///
/// ponytail: Latin-1 fallback, read PR_INTERNET_CPID if a legacy non-western
/// .msg ever shows up.
fn eight_bit(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| e.as_bytes().iter().map(|&c| c as char).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    const EML: &[u8] = b"From: =?utf-8?Q?J=C3=BCrgen_Wei=C3=9F?= <j@example.com>\r\n\
To: Alice <alice@example.com>, bob@example.com\r\n\
Cc: Carol <carol@example.com>\r\n\
Date: Sat, 20 Nov 2021 14:22:01 -0800\r\n\
Subject: Quarterly =?utf-8?B?cmV2aWV3?=\r\n\
Content-Type: multipart/mixed; boundary=\"sep\"\r\n\
\r\n\
--sep\r\n\
Content-Type: text/html; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
<h2>Numbers</h2><p>Margin is <b>up</b> 4=25.</p>\r\n\
--sep\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--sep--\r\n";

    #[test]
    fn eml_headers_are_decoded_and_addresses_joined() {
        let mail = eml_mail(EML).unwrap();
        // RFC 2047 encoded words in both the subject and the display name.
        assert_eq!(mail.subject, "Quarterly review");
        assert_eq!(mail.from, "Jürgen Weiß <j@example.com>");
        assert_eq!(mail.to, "Alice <alice@example.com>, bob@example.com");
        assert_eq!(mail.cc, "Carol <carol@example.com>");
        assert!(
            mail.date.starts_with("2021-11-20T14:22:01"),
            "{}",
            mail.date
        );
    }

    #[test]
    fn eml_html_body_is_reduced_to_markdown() {
        // quoted-printable decoded (`4=25` → `4%`), then HTML → markdown.
        let mail = eml_mail(EML).unwrap();
        assert!(mail.body.contains("## Numbers"), "{}", mail.body);
        assert!(mail.body.contains("**up**"), "{}", mail.body);
        assert!(mail.body.contains("4%"), "{}", mail.body);
    }

    #[test]
    fn eml_attachments_are_listed_with_decoded_size() {
        let mail = eml_mail(EML).unwrap();
        // "JVBERi0xLjQK" is 12 base64 chars → 8 decoded bytes ("%PDF-1.4\n").
        assert_eq!(mail.attachments, vec![("report.pdf".to_string(), 9)]);
    }

    #[test]
    fn eml_plain_text_body_is_kept_verbatim() {
        let raw = b"Subject: Re: lunch\r\n\r\n> where?\r\n\r\nthe usual place\r\n";
        let mail = eml_mail(raw).unwrap();
        assert_eq!(mail.body, "> where?\r\n\r\nthe usual place\r\n");
    }

    /// A minimal but real .msg: the CFB container Outlook writes, carrying the
    /// property streams this module reads.
    fn sample_msg() -> Cursor<Vec<u8>> {
        let unicode =
            |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_le_bytes).collect() };
        let mut c = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        for (id, value) in [
            (P_SUBJECT, "Kickoff notes"),
            (P_SENDER_NAME, "Jürgen Weiß"),
            (P_SENDER_EMAIL, "j@example.com"),
            (P_DISPLAY_TO, "Alice; Bob"),
            (P_BODY, "See the deck."),
            (
                P_TRANSPORT_HEADERS,
                "Date: Sat, 20 Nov 2021 14:22:01 -0800\r\n",
            ),
        ] {
            let mut s = c
                .create_stream(format!("/{}", stream_name(id, "001F")))
                .unwrap();
            s.write_all(&unicode(value)).unwrap();
        }
        c.create_storage("/__attach_version1.0_#00000000").unwrap();
        let mut s = c
            .create_stream(format!(
                "/__attach_version1.0_#00000000/{}",
                stream_name(P_ATTACH_LONG_NAME, "001F")
            ))
            .unwrap();
        s.write_all(&unicode("deck.pdf")).unwrap();
        // Each stream must be dropped (flushed) before the container is taken
        // apart, a still-open stream never reaches the bytes.
        drop(s);
        let mut s = c
            .create_stream(format!(
                "/__attach_version1.0_#00000000/{}",
                stream_name(P_ATTACH_DATA, "0102")
            ))
            .unwrap();
        s.write_all(&[0u8; 128]).unwrap();
        drop(s);
        c.flush().unwrap();
        c.into_inner()
    }

    #[test]
    fn msg_properties_become_the_same_mail() {
        let comp = cfb::CompoundFile::open(sample_msg()).unwrap();
        let mail = msg_mail(comp).unwrap();
        assert_eq!(mail.subject, "Kickoff notes");
        assert_eq!(mail.from, "Jürgen Weiß <j@example.com>");
        assert_eq!(mail.to, "Alice; Bob");
        assert_eq!(mail.body, "See the deck.");
        // The date comes from the transport headers, parsed as RFC 5322.
        assert!(
            mail.date.starts_with("2021-11-20T14:22:01"),
            "{}",
            mail.date
        );
        assert_eq!(mail.attachments, vec![("deck.pdf".to_string(), 128)]);
    }

    #[test]
    fn utf16_and_eight_bit_decoders() {
        assert_eq!(utf16le(&[0x41, 0x00, 0xDF, 0x00]), "Aß");
        // Odd trailing byte is dropped rather than panicking.
        assert_eq!(utf16le(&[0x41, 0x00, 0x42]), "A");
        assert_eq!(eight_bit("Grüße".as_bytes().to_vec()), "Grüße");
        // Not UTF-8: Latin-1 keeps the umlaut readable.
        assert_eq!(eight_bit(vec![b'G', b'r', 0xFC, b'n']), "Grün");
    }

    #[test]
    fn sample_messages_open_and_yield_their_inline_image() {
        // Both containers, end to end from a real file on disk.
        for path in ["samples/sample.eml", "samples/sample.msg"] {
            let md = to_markdown(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert!(md.starts_with("# Quartalsbericht Q2,"), "{path}: {md}");
            assert!(md.contains("**From:** Jürgen Weiß"), "{path}: {md}");
            assert!(md.contains("- bericht.pdf"), "{path}: {md}");
            // The PNG attachment reaches the viewer's gallery as a real file;
            // the PDF one does not, no decoder for it.
            let media = media(path);
            assert_eq!(media.len(), 1, "{path}: {media:?}");
            assert!(media[0].exists(), "{path}: {media:?}");
        }
    }

    #[test]
    fn render_shows_subject_envelope_body_and_attachments() {
        let md = Mail {
            subject: "Hi".into(),
            from: "a@example.com".into(),
            to: "b@example.com".into(),
            body: "text".into(),
            attachments: vec![("a.pdf".into(), 2048)],
            ..Default::default()
        }
        .render();
        assert!(md.starts_with("# Hi\n\n"), "{md}");
        // Hard breaks (two trailing spaces) keep the envelope on separate lines.
        assert!(md.contains("**From:** a@example.com  \n"), "{md}");
        assert!(md.contains("**To:** b@example.com  \n"), "{md}");
        // Empty envelope fields are omitted, not shown blank.
        assert!(!md.contains("**Cc:**"), "{md}");
        assert!(!md.contains("**Date:**"), "{md}");
        assert!(md.contains("- a.pdf  ·  2.0K"), "{md}");
    }

    #[test]
    fn render_of_an_empty_message_is_still_a_document() {
        let md = Mail::default().render();
        assert!(md.contains("# (no subject)"), "{md}");
        assert!(md.contains("*(no body)*"), "{md}");
        assert!(!md.contains("Attachments"), "{md}");
    }
}
