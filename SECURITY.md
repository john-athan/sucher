# Security Policy

## Reporting a vulnerability

Please report security issues privately via GitHub's
[private vulnerability reporting](https://github.com/john-athan/sucher/security/advisories/new)
rather than opening a public issue. I'll aim to respond within a few days.

## Scope notes

sucher opens files you point it at and, for some formats, shells out to local
tools (`pdftocairo`, `pdftotext`, `ffmpeg`, `ffprobe`). External commands are
invoked with arguments passed directly — never through a shell — so file names
cannot inject commands.

Treat untrusted documents with the same caution as any viewer: a malicious file
exercises the upstream parsers (`image`, `calamine`, poppler, ffmpeg, etc.).
Opening links from a markdown document hands the URL to your OS default handler.
