//! Outline origins omitted by Swash's sbix bitmap placement.
use cosmic_text::ttf_parser::{Face, GlyphId};
use swash::scale::{Source, image::Image};

pub(crate) fn correct_origin(
    image: &mut Image,
    face: &Face<'_>,
    glyph_id: u16,
    device_pixel_size: f32,
) {
    // sbix bitmap offsets are relative to the lower-left outline corner when
    // a contour exists. Match Skia: hmtx side bearing and glyf yMin, scaled
    // from design units. CBDT and outline glyphs already have baseline origins.
    // https://learn.microsoft.com/en-us/typography/opentype/spec/sbix
    if !matches!(image.source, Source::ColorBitmap(_)) || face.tables().sbix.is_none() {
        return;
    }
    let glyph = GlyphId(glyph_id);
    if let Some(bounds) = face.glyph_bounding_box(glyph) {
        let scale = device_pixel_size / f32::from(face.units_per_em());
        image.placement.left +=
            (f32::from(face.glyph_hor_side_bearing(glyph).unwrap_or(0)) * scale).round() as i32;
        image.placement.top += (f32::from(bounds.y_min) * scale).round() as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swash::scale::StrikeWith;
    const FONT: &[u8] = include_bytes!("test_fixtures/sbix-outline-origin.ttf");

    #[test]
    fn bitmap_origin_includes_outline_y_min_at_device_scale() {
        let face = Face::parse(FONT, 0).expect("synthetic sbix fixture");
        let mut image = Image::new();
        image.source = Source::ColorBitmap(StrikeWith::BestFit);
        image.placement.top = 54;
        correct_origin(&mut image, &face, 0, 38.5);
        assert_eq!(image.placement.top, 35);
        assert_eq!(image.placement.left, 0);
    }

    #[test]
    fn monochrome_outline_placement_is_unchanged() {
        let face = Face::parse(FONT, 0).expect("synthetic sbix fixture");
        let mut image = Image::new();
        image.source = Source::Outline;
        image.placement.top = 29;
        correct_origin(&mut image, &face, 0, 38.5);
        assert_eq!(image.placement.top, 29);
    }
}
