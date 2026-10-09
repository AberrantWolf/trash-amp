//! Collector cache filenames are hashes without image extensions.
use std::path::Path;

pub fn load(path: &Path) -> Option<slint::Image> {
    let pixels = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .into_rgba8();
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        pixels.as_raw(),
        pixels.width(),
        pixels.height(),
    );
    Some(slint::Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn loads_extensionless_cached_artwork() {
        for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg] {
            let pixels = image::RgbImage::from_pixel(2, 3, image::Rgb([20, 40, 60]));
            let mut bytes = Cursor::new(Vec::new());
            pixels.write_to(&mut bytes, format).unwrap();
            let path = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
            std::fs::write(&path, bytes.into_inner()).unwrap();
            let loaded = load(&path);
            std::fs::remove_file(&path).unwrap();
            let size = loaded.unwrap().size();
            assert_eq!((size.width, size.height), (2, 3));
        }
    }

    #[test]
    fn invalid_artwork_falls_back() {
        let path = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::write(&path, b"not an image").unwrap();
        assert!(load(&path).is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(load(&path).is_none());
    }
}
