use crate::Space;
use exif::experimental::Writer;
use exif::{Context, Field, In, Reader, Tag, Value};
use std::io::Cursor;

const KEEP: &[Tag] = &[
    Tag::Make, Tag::Model, Tag::DateTime, Tag::Artist, Tag::Copyright, Tag::ImageDescription,
    Tag::ExposureTime, Tag::FNumber, Tag::ExposureProgram, Tag::PhotographicSensitivity, Tag::DateTimeOriginal,
    Tag::DateTimeDigitized, Tag::OffsetTime, Tag::OffsetTimeOriginal, Tag::ShutterSpeedValue, Tag::ApertureValue,
    Tag::ExposureBiasValue, Tag::MaxApertureValue, Tag::MeteringMode, Tag::LightSource, Tag::Flash,
    Tag::FocalLength, Tag::ExposureMode, Tag::WhiteBalance, Tag::FocalLengthIn35mmFilm, Tag::SceneCaptureType,
    Tag::LensMake, Tag::LensModel, Tag::LensSpecification, Tag::BodySerialNumber, Tag::LensSerialNumber,
    Tag::SensitivityType, Tag::ExifVersion,
];

pub fn build(raw: &[u8], w: u32, h: u32, space: Space) -> Option<Vec<u8>> {
    let src = Reader::new().continue_on_error(true).read_from_container(&mut Cursor::new(raw)).or_else(|e| e.distill_partial_result(|_| {})).ok()?;
    let mut fields: Vec<Field> = src
        .fields()
        .filter(|f| f.ifd_num == In::PRIMARY && (KEEP.contains(&f.tag) || f.tag.0 == Context::Gps))
        .cloned()
        .collect();
    let mut add = |tag, v| fields.push(Field { tag, ifd_num: In::PRIMARY, value: v });
    add(Tag::Orientation, Value::Short(vec![1]));
    add(Tag::ColorSpace, Value::Short(vec![if space == Space::Srgb { 1 } else { 0xFFFF }]));
    add(Tag::PixelXDimension, Value::Long(vec![w]));
    add(Tag::PixelYDimension, Value::Long(vec![h]));
    add(Tag::Software, Value::Ascii(vec![b"PhotoVault".to_vec()]));
    let mut wr = Writer::new();
    for f in &fields {
        wr.push_field(f);
    }
    let mut out = Cursor::new(Vec::new());
    wr.write(&mut out, false).ok()?;
    let v = out.into_inner();
    (v.len() <= 65_000).then_some(v)
}
