// sucher — a fast terminal viewer for files that are awkward in a browser:
// markdown, source/plain text, spreadsheets (incl. csv/tsv), PDF, images, video,
// docx, pptx, Keynote, archives, binary (hex), and directories. One command
// dispatches by a single classification registry (`format.rs`) to a per-type
// viewer.
//
// Interactive TUI when stdout is a tty; falls back to a one-shot text dump when
// piped or with --plain (markdown can use the kitty text-sizing protocol for
// big headings). The few still-unopenable files (legacy .doc/.ppt binaries,
// audio) print a metadata line rather than being force-rendered.

mod archive;
mod config;
mod dir;
mod docx;
mod format;
mod git;
mod hex;
mod highlight;
mod icons;
mod imgview;
mod keynote;
mod markdown;
mod media;
mod pdf;
mod plain;
mod pptx;
mod query;
mod sheet;
mod svg;
mod text;
mod theme;
mod tui;
mod typeahead;
mod util;
mod video;
mod xlsx;

use format::Format;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::{env, fs};

fn main() -> ExitCode {
    let mut plain_flag = false;
    let mut path: Option<String> = None;
    // Theme/icons overrides from the command line (highest precedence — see
    // `config::load`). Both flags take the following argument.
    let mut cli_theme: Option<String> = None;
    let mut cli_icons: Option<String> = None;
    let mut cli_layout: Option<String> = None;
    // `--no-git` forces the git gutter off, overriding env/file/default.
    let mut cli_no_git = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plain" | "-p" => plain_flag = true,
            "--theme" => cli_theme = args.next(),
            "--icons" => cli_icons = args.next(),
            "--layout" => cli_layout = args.next(),
            "--no-git" => cli_no_git = true,
            "-h" | "--help" => {
                eprintln!(
                    "usage: sucher [--plain] [--theme NAME] [--icons unicode|nerd|none] [--layout auto|miller|double] [--no-git] [file|dir]"
                );
                return ExitCode::SUCCESS;
            }
            _ => path = Some(arg),
        }
    }

    // Resolve the palette (flag > env > file > default) and install it before
    // any viewer draws. Auto light/dark detection runs here, before the
    // alternate screen. `icons` threads through to the browser for a later phase.
    let cli_git = if cli_no_git { Some(false) } else { None };
    let config = config::load(cli_theme, cli_icons, cli_layout, cli_git);
    theme::init(config.palette);

    // No argument browses the current directory.
    let path = path.unwrap_or_else(|| ".".to_string());

    let interactive = !plain_flag && io::stdout().is_terminal();
    let title = file_title(&path);

    match format::classify_path(Path::new(&path)) {
        // Directories open the file browser (or a plain listing when piped).
        Format::Directory => {
            if interactive {
                if let Err(e) = dir::run(path, config.icons, config.layout, config.git) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&dir::dump(&path));
            }
        }
        Format::Image => {
            if interactive {
                if let Err(e) = imgview::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                match image::image_dimensions(&path) {
                    Ok((w, h)) => return emit(&format!("{path}: image {w}×{h}px\n")),
                    Err(e) => {
                        eprintln!("sucher: {path}: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
        }
        Format::Sheet => {
            if interactive {
                if let Err(e) = sheet::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&sheet::dump(&path));
            }
        }
        Format::Svg => {
            if interactive {
                if let Err(e) = svg::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                // Piped: SVG is XML source — dump it faithfully like any text.
                return emit(&text::dump(&path));
            }
        }
        Format::Pdf => {
            if interactive {
                if let Err(e) = pdf::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&pdf::dump(&path));
            }
        }
        Format::Video => {
            if interactive {
                if let Err(e) = video::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&video::dump(&path));
            }
        }
        Format::Docx => {
            let src = match docx::to_markdown(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("sucher: {path}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let images = if interactive {
                docx::media(&path)
            } else {
                Vec::new()
            };
            return render_markdown(interactive, title, src, images);
        }
        Format::Pptx => {
            let src = match pptx::to_markdown(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("sucher: {path}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let images = if interactive {
                pptx::media(&path)
            } else {
                Vec::new()
            };
            return render_markdown(interactive, title, src, images);
        }
        Format::Keynote => {
            if interactive {
                if let Err(e) = keynote::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&format!("{path}: Keynote presentation\n"));
            }
        }
        Format::Archive => {
            if interactive {
                if let Err(e) = archive::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&archive::dump(&path));
            }
        }
        Format::Binary => {
            if interactive {
                if let Err(e) = hex::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&hex::dump(&path));
            }
        }
        Format::Text => {
            if interactive {
                if let Err(e) = text::run(title, path.clone()) {
                    eprintln!("sucher: {e}");
                    return ExitCode::FAILURE;
                }
            } else {
                return emit(&text::dump(&path));
            }
        }
        Format::Markdown => {
            let src = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("sucher: {path}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            return render_markdown(interactive, title, src, Vec::new());
        }
        // Recognized but still unopenable (legacy office binaries, audio): show
        // a metadata line, never feed the bytes to a renderer.
        f @ (Format::Doc | Format::Audio) => {
            return unsupported(&path, f, interactive);
        }
    }
    ExitCode::SUCCESS
}

/// A recognized-but-unopenable file: print a concise, honest "no viewer" notice
/// with one metadata line (size + modified). Interactive callers get the notice
/// on stderr; piped callers get just the metadata line on stdout so it composes.
/// Lacking a viewer is not an error, so this returns SUCCESS.
fn unsupported(path: &str, format: Format, interactive: bool) -> ExitCode {
    let name = file_title(path);
    let meta = metadata_line(path);
    if interactive {
        eprintln!("sucher: no viewer for {} ({name})", format.label());
        eprintln!("  {meta}");
        ExitCode::SUCCESS
    } else {
        emit(&format!("{meta}\n"))
    }
}

/// Write a one-shot dump to stdout for piped/non-TTY output. A closed downstream
/// pipe (`v big.md | head`) is a normal, clean exit — treat `BrokenPipe` as
/// success rather than letting the `print!` macro panic on it. The buffer is
/// flushed here so there is no late broken-pipe panic during process teardown.
fn emit(s: &str) -> ExitCode {
    use std::io::Write;
    let mut out = io::stdout();
    match out.write_all(s.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sucher: {e}");
            ExitCode::FAILURE
        }
    }
}

/// One-line "size · modified" summary for a path, human-formatted.
fn metadata_line(path: &str) -> String {
    match fs::metadata(path) {
        Ok(m) => {
            let mut s = util::human_size(m.len());
            if let Ok(t) = m.modified() {
                s.push_str(&format!("  ·  {}", util::rel_time(t)));
            }
            s
        }
        Err(e) => format!("({e})"),
    }
}

fn render_markdown(
    interactive: bool,
    title: String,
    src: String,
    images: Vec<std::path::PathBuf>,
) -> ExitCode {
    if interactive {
        if let Err(e) = tui::run(title, src, images) {
            eprintln!("sucher: {e}");
            return ExitCode::FAILURE;
        }
    } else {
        return emit(&plain::render(&src));
    }
    ExitCode::SUCCESS
}

/// File name used as a viewer title.
fn file_title(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Open a path in its interactive viewer. Used by the directory browser, which
/// has already torn down its own terminal; each viewer sets up and restores its
/// own. Errors are printed but never abort the caller.
pub fn open_interactive(path: &str) {
    let title = file_title(path);
    let format = format::classify_path(Path::new(path));
    let r = match format {
        Format::Image => imgview::run(title, path.to_string()),
        Format::Svg => svg::run(title, path.to_string()),
        Format::Sheet => sheet::run(title, path.to_string()),
        Format::Pdf => pdf::run(title, path.to_string()),
        Format::Video => video::run(title, path.to_string()),
        Format::Text => text::run(title, path.to_string()),
        Format::Docx => match docx::to_markdown(path) {
            Ok(src) => tui::run(title, src, docx::media(path)),
            Err(e) => {
                eprintln!("sucher: {path}: {e}");
                Ok(())
            }
        },
        Format::Pptx => match pptx::to_markdown(path) {
            Ok(src) => tui::run(title, src, pptx::media(path)),
            Err(e) => {
                eprintln!("sucher: {path}: {e}");
                Ok(())
            }
        },
        Format::Keynote => keynote::run(title, path.to_string()),
        Format::Archive => archive::run(title, path.to_string()),
        Format::Binary => hex::run(title, path.to_string()),
        Format::Markdown => match fs::read_to_string(path) {
            Ok(src) => tui::run(title, src, Vec::new()),
            Err(e) => {
                eprintln!("sucher: {path}: {e}");
                Ok(())
            }
        },
        // Directories don't reach here (the browser enters them itself); the
        // remaining variants have no viewer. The browser gates these before
        // calling, but stay honest if reached directly.
        Format::Directory | Format::Doc | Format::Audio => {
            eprintln!("sucher: no viewer for {} ({title})", format.label());
            Ok(())
        }
    };
    if let Err(e) = r {
        eprintln!("sucher: {e}");
    }
}
