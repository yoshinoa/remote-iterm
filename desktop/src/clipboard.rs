use gpui::{Image, ImageFormat};

pub fn read_bitmap() -> Option<Image> {
    let image = arboard::Clipboard::new().ok()?.get_image().ok()?;
    let png = png_from_rgba(image.width as u32, image.height as u32, image.bytes.into_owned())?;
    Some(Image::from_bytes(ImageFormat::Png, png))
}

pub fn png_from_rgba(width: u32, height: u32, mut rgba: Vec<u8>) -> Option<Vec<u8>> {
    if rgba.chunks_exact(4).all(|px| px[3] == 0) {
        rgba.chunks_exact_mut(4).for_each(|px| px[3] = 255);
    }
    let image = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png: &[u8]) -> Vec<u8> {
        image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap().to_rgba8().into_raw()
    }

    #[test]
    fn rgba_round_trips_through_png() {
        let rgba = vec![10, 20, 30, 255, 40, 50, 60, 128];
        assert_eq!(decode(&png_from_rgba(2, 1, rgba.clone()).unwrap()), rgba);
    }

    #[test]
    fn fully_transparent_bitmap_becomes_opaque() {
        let png = png_from_rgba(2, 1, vec![10, 20, 30, 0, 40, 50, 60, 0]).unwrap();
        assert_eq!(decode(&png), vec![10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn mismatched_size_is_rejected() {
        assert_eq!(png_from_rgba(2, 2, vec![0; 8]), None);
    }
}
