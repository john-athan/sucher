// Keynote (.key) preview.
//
// A modern .key is an iWork *package* — a zip whose real content is IWA
// (Snappy-compressed protobuf) we deliberately don't parse. But Keynote embeds a
// QuickLook JPEG of the deck so Finder can show a thumbnail; we extract the
// best available one and hand it to the image viewer. This gives an honest
// visual preview (the first slide / cover) with zero format decoding.
//
// Full per-slide text would mean decoding the IWA protobuf stream — out of scope;
// see the effort discussion in the project notes.

use image::DynamicImage;

/// Candidate preview part names inside a .key package, best (largest / most
/// representative) first. Names have varied across Keynote versions.
const PREVIEW_PARTS: &[&str] = &[
    "preview.jpg",
    "preview-web.jpg",
    "QuickLook/Thumbnail.jpg",
    "preview-micro.jpg",
];

pub fn run(title: String, path: String) -> std::io::Result<()> {
    match preview_image(&path) {
        // `x` in the viewer opens the .key package itself (Keynote), not the
        // extracted preview JPEG.
        Ok(img) => crate::imgview::show(title, img, Some(path)),
        Err(e) => {
            eprintln!("sucher: {path}: {e}");
            Ok(())
        }
    }
}

/// Decode the embedded QuickLook preview of a .key package. Errors when the file
/// is not a readable zip or carries no recognised preview part.
pub fn preview_image(path: &str) -> Result<DynamicImage, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for part in PREVIEW_PARTS {
        if let Ok(mut f) = zip.by_name(part) {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut bytes).map_err(|e| e.to_string())?;
            return image::load_from_memory(&bytes).map_err(|e| e.to_string());
        }
    }
    Err("no embedded preview image in this Keynote file".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// Write a zip containing the given (name, bytes) parts to a unique temp path.
    /// `tag` disambiguates concurrent tests sharing this process.
    fn write_zip(tag: &str, parts: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sucher-keytest-{}-{tag}.key", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        for (name, bytes) in parts {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap();
        path
    }

    /// A JPEG-encoded solid image of the given size, as bytes.
    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::new_rgb8(w, h);
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        bytes
    }

    #[test]
    fn decodes_embedded_preview() {
        let path = write_zip(
            "decode",
            &[("Index.zip", b"iwa".to_vec()), ("preview.jpg", jpeg(12, 8))],
        );
        let img = preview_image(path.to_str().unwrap()).expect("should decode");
        assert_eq!((img.width(), img.height()), (12, 8));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn errors_when_no_preview_part() {
        let path = write_zip("nopreview", &[("Index.zip", b"iwa".to_vec())]);
        assert!(preview_image(path.to_str().unwrap()).is_err());
        std::fs::remove_file(path).ok();
    }
}
