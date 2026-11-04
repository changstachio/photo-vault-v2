#![forbid(unsafe_code)]
pub mod exifcopy;
pub mod icc;
pub mod recompress;
pub mod types;

use anyhow::{bail, Context, Result};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
pub use types::{Chroma, ColourMode, Render, WhiteBalance};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    Srgb,
    AdobeRgb,
    ProPhoto,
}

impl Space {
    fn libraw(self) -> i32 {
        match self {
            Space::Srgb => 1,
            Space::AdobeRgb => 2,
            Space::ProPhoto => 4,
        }
    }
    fn gamma(self) -> (f64, f64) {
        match self {
            Space::Srgb => (1.0 / 2.4, 12.92),
            Space::AdobeRgb => (256.0 / 563.0, 0.0),
            Space::ProPhoto => (1.0 / 1.8, 0.0),
        }
    }
}

pub struct Converted {
    pub width: u32,
    pub height: u32,
    pub size: u64,
    pub hash: [u8; 32],
    pub space: Space,
}

pub fn convert(arw: &Path, out: &Path, mode: ColourMode, demosaic: i32, render: &Render) -> Result<Converted> {
    let (c, jpg) = render_jpg(arw, mode, demosaic, render)?;
    write_atomic(out, &jpg)?;
    Ok(c)
}

struct Rendered {
    px: Vec<u8>,
    w: u32,
    h: u32,
    space: Space,
    exif: Option<Vec<u8>>,
}

/// LibRaw render of `arw` to 8-bit RGB (the slow part; shared by every output setting).
fn render_pixels(arw: &Path, mode: ColourMode, demosaic: i32, render: &Render) -> Result<Rendered> {
    let buf = std::fs::read(arw).with_context(|| format!("read {}", arw.display()))?;
    let mut raw = libraw_lite::Raw::open(&buf).map_err(|e| anyhow::anyhow!("open raw: {e}"))?;
    let space = match mode {
        ColourMode::Srgb => Space::Srgb,
        ColourMode::Adobergb => Space::AdobeRgb,
        ColourMode::Prophoto => Space::ProPhoto,
        ColourMode::Preserve => match raw.exif_colorspace() {
            1 => Space::Srgb,
            _ => Space::AdobeRgb,
        },
    };
    raw.set_output_color(space.libraw());
    let (g0, g1) = space.gamma();
    raw.set_gamma(g0 as f32, g1 as f32);
    raw.set_camera_wb(render.white_balance == WhiteBalance::Camera);
    raw.set_demosaic(demosaic);
    raw.set_render(&libraw_lite::RenderParams {
        auto_wb: render.white_balance == WhiteBalance::Auto,
        exp_shift: 2f32.powf(render.exposure_ev),
        exp_preser: render.preserve_highlights,
        bright: render.brightness,
        auto_bright: render.auto_bright,
        auto_bright_thr: render.auto_bright_clip_pct / 100.0,
        highlight: render.highlight as i32,
        threshold: render.noise_threshold,
        fbdd_noiserd: render.fbdd_noise as i32,
        med_passes: render.median_passes as i32,
    });
    raw.unpack().map_err(|e| anyhow::anyhow!("unpack: {e}"))?;
    let img = raw.process_rgb8().map_err(|e| anyhow::anyhow!("process: {e}"))?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
        bail!("unexpected image size {w}x{h}");
    }
    let px = img.pixels().to_vec();
    drop(img);
    let exif = exifcopy::build(&buf, w, h, space);
    Ok(Rendered { px, w, h, space, exif })
}

fn encode_rendered(r: &Rendered, quality: u8, chroma: Chroma) -> Result<Vec<u8>> {
    let mut jpg = Vec::with_capacity(r.px.len() / 4);
    let mut enc = Encoder::new(&mut jpg, quality.clamp(1, 100));
    set_sampling(&mut enc, chroma);
    enc.add_icc_profile(&icc::profile(r.space)).map_err(|e| anyhow::anyhow!("icc: {e}"))?;
    if let Some(x) = &r.exif {
        let _ = enc.add_exif_metadata(x);
    }
    enc.encode(&r.px, r.w as u16, r.h as u16, ColorType::Rgb).map_err(|e| anyhow::anyhow!("encode: {e}"))?;
    Ok(jpg)
}

/// Chroma subsampling plus Huffman table choice. jpeg-encoder 0.7's optimised Huffman tables
/// combined with vertically subsampled chroma (4:2:0) produce files that libjpeg reads correctly
/// but zune-jpeg (used by many image viewers) decodes as garbage, so 4:2:0 uses the standard tables.
pub(crate) fn set_sampling<W: jpeg_encoder::JfifWrite>(enc: &mut Encoder<W>, chroma: Chroma) {
    enc.set_sampling_factor(match chroma {
        Chroma::C444 => SamplingFactor::R_4_4_4,
        Chroma::C422 => SamplingFactor::R_4_2_2,
        Chroma::C420 => SamplingFactor::R_4_2_0,
    });
    enc.set_optimized_huffman_tables(chroma != Chroma::C420);
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let (mut sum, mut n) = (0u64, 0u64);
    for i in (0..a.len().min(b.len())).step_by(7) {
        sum += (a[i] as i32 - b[i] as i32).unsigned_abs() as u64;
        n += 1;
    }
    sum as f64 / n.max(1) as f64
}

fn zune_decode(jpg: &[u8]) -> Result<Vec<u8>> {
    let mut d = zune_jpeg::JpegDecoder::new(zune_jpeg::zune_core::bytestream::ZCursor::new(jpg));
    d.decode().map_err(|e| anyhow::anyhow!("{e:?}"))
}

fn ref_decode(jpg: &[u8]) -> Result<Vec<u8>> {
    let mut d = jpeg_decoder::Decoder::new(jpg);
    d.decode().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Largest mean per-channel error accepted between a decoded JPG and the pixels it was made from.
/// Normal JPEG loss is a few units even at low quality; a broken file is 50-120.
const MAX_MEAN_ERROR: f64 = 20.0;

/// Decode `jpg` with two independent decoders (jpeg-decoder and zune-jpeg) and require both to
/// match `src` (the pixels it was encoded from). Catches files that some viewers show wrongly.
pub fn verify_against(jpg: &[u8], src: &[u8]) -> Result<()> {
    for (name, px) in [("jpeg-decoder", ref_decode(jpg)), ("zune-jpeg", zune_decode(jpg))] {
        let px = px.with_context(|| format!("{name} cannot decode the new JPG"))?;
        if px.len() != src.len() {
            bail!("{name} decodes the new JPG to {} bytes, expected {}", px.len(), src.len());
        }
        let e = mean_abs_diff(&px, src);
        if e > MAX_MEAN_ERROR {
            bail!("{name} decodes the new JPG wrongly (mean error {e:.1}); not written");
        }
    }
    Ok(())
}

/// True if the two decoders disagree on `jpg` (a file some viewers show as garbage).
pub fn decoders_disagree(jpg: &[u8]) -> Result<bool> {
    let a = ref_decode(jpg)?;
    let b = zune_decode(jpg)?;
    Ok(a.len() != b.len() || mean_abs_diff(&a, &b) > MAX_MEAN_ERROR)
}

/// Pixels as decoded by jpeg-decoder (matches libjpeg); used to prove a repair is lossless.
pub fn reference_pixels(jpg: &[u8]) -> Result<Vec<u8>> {
    ref_decode(jpg)
}

/// Baseline JPG whose chroma is subsampled vertically: the only kind the encoder bug produced.
pub fn vertically_subsampled_baseline(j: &[u8]) -> bool {
    if j.len() < 4 || j[0] != 0xFF || j[1] != 0xD8 {
        return false;
    }
    let mut i = 2;
    while i + 4 <= j.len() && j[i] == 0xFF {
        let m = j[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        if m == 0xDA || m == 0xD9 {
            return false;
        }
        let len = u16::from_be_bytes([j[i + 2], j[i + 3]]) as usize;
        if len < 2 || i + 2 + len > j.len() {
            return false;
        }
        if m == 0xC0 || m == 0xC1 {
            // SOFn: P(1) Y(2) X(2) Nf(1), then per component: id, HiVi, Tq
            let s = &j[i + 4..i + 2 + len];
            if s.len() < 9 || s[5] < 3 {
                return false;
            }
            let (hv_y, hv_cb) = (s[7], s[10]);
            return (hv_y & 0x0F) > (hv_cb & 0x0F);
        }
        i += 2 + len;
    }
    false
}

/// Render `arw` to JPG bytes in memory without touching the disk.
pub fn render_jpg(arw: &Path, mode: ColourMode, demosaic: i32, render: &Render) -> Result<(Converted, Vec<u8>)> {
    let r = render_pixels(arw, mode, demosaic, render)?;
    let jpg = encode_rendered(&r, render.quality, render.chroma)?;
    verify_against(&jpg, &r.px)?;
    let hash = *blake3::hash(&jpg).as_bytes();
    let size = jpg.len() as u64;
    Ok((Converted { width: r.w, height: r.h, size, hash, space: r.space }, jpg))
}

/// JPG size for each (quality, chroma) in `variants`, from a single LibRaw render of `arw`
/// (encodes run in parallel). Returns (pixel width, pixel height, sizes in the same order).
pub fn size_table(arw: &Path, mode: ColourMode, demosaic: i32, render: &Render, variants: &[(u8, Chroma)]) -> Result<(u32, u32, Vec<usize>)> {
    let r = render_pixels(arw, mode, demosaic, render)?;
    let sizes = parallel_sizes(variants, |q, c| encode_rendered(&r, q, c).map(|j| j.len()))?;
    Ok((r.w, r.h, sizes))
}

/// JPG size for each (quality, chroma) when an existing JPG is re-encoded.
pub fn size_table_jpg(jpg: &[u8], variants: &[(u8, Chroma)]) -> Result<(u32, u32, Vec<usize>)> {
    let mut d = jpeg_decoder::Decoder::new(jpg);
    let px = d.decode().map_err(|e| anyhow::anyhow!("{e}"))?;
    let info = d.info().context("no image info")?;
    let sizes = parallel_sizes(variants, |q, c| recompress::recompress_pixels(jpg, &px, &info, q, c).map(|j| j.len()))?;
    Ok((info.width as u32, info.height as u32, sizes))
}

fn parallel_sizes(variants: &[(u8, Chroma)], f: impl Fn(u8, Chroma) -> Result<usize> + Sync) -> Result<Vec<usize>> {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let mut out = vec![0usize; variants.len()];
    let f = &f;
    std::thread::scope(|s| -> Result<()> {
        let hs: Vec<_> = out.chunks_mut(variants.len().div_ceil(n).max(1)).zip(variants.chunks(variants.len().div_ceil(n).max(1))).map(|(o, v)| s.spawn(move || -> Result<()> { for (slot, (q, c)) in o.iter_mut().zip(v) { *slot = f(*q, *c)?; } Ok(()) })).collect();
        for h in hs {
            h.join().map_err(|_| anyhow::anyhow!("encoder thread panicked"))??;
        }
        Ok(())
    })?;
    Ok(out)
}

pub struct JpgInfo {
    pub width: u32,
    pub height: u32,
    pub has_icc: bool,
}

pub fn probe_decode(p: &Path) -> Result<JpgInfo> {
    let f = std::io::BufReader::new(std::fs::File::open(p)?);
    probe(f).with_context(|| format!("decode {}", p.display()))
}

/// Same checks as [`probe_decode`], on JPG bytes still in memory.
pub fn probe_decode_bytes(jpg: &[u8]) -> Result<JpgInfo> {
    probe(jpg).context("decode")
}

fn probe(r: impl std::io::Read) -> Result<JpgInfo> {
    let mut d = jpeg_decoder::Decoder::new(r);
    d.decode().map_err(|e| anyhow::anyhow!("{e}"))?;
    let i = d.info().context("no info")?;
    Ok(JpgInfo { width: i.width as u32, height: i.height as u32, has_icc: d.icc_profile().is_some() })
}

pub fn thumbnail(p: &Path, max: u16) -> Result<(usize, usize, Vec<u8>)> {
    let f = std::io::BufReader::new(std::fs::File::open(p)?);
    let mut d = jpeg_decoder::Decoder::new(f);
    d.read_info().map_err(|e| anyhow::anyhow!("{e}"))?;
    d.scale(max, max).map_err(|e| anyhow::anyhow!("{e}"))?;
    let px = d.decode().map_err(|e| anyhow::anyhow!("{e}"))?;
    let i = d.info().context("no info")?;
    let rgb = match i.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => px,
        jpeg_decoder::PixelFormat::L8 => px.iter().flat_map(|&v| [v, v, v]).collect(),
        _ => bail!("unsupported pixel format"),
    };
    Ok((i.width as usize, i.height as usize, rgb))
}

/// Test helper: a JPG encoded with the settings that triggered the jpeg-encoder bug.
#[doc(hidden)]
pub fn test_buggy_jpg(px: &[u8], w: u16, h: u16) -> Vec<u8> {
    let mut b = Vec::new();
    let mut e = Encoder::new(&mut b, 90);
    e.set_sampling_factor(SamplingFactor::R_4_2_0);
    e.set_optimized_huffman_tables(true);
    e.encode(px, w, h, ColorType::Rgb).unwrap();
    b
}

#[doc(hidden)]
pub mod testdng {
    pub fn make(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 2) as usize);
        for y in 0..h {
            for x in 0..w {
                let red_side = x < w / 2;
                let (r, g, b) = if red_side { (12000u16, 3000, 1000) } else { (1000, 3000, 12000) };
                let v = match (y % 2, x % 2) {
                    (0, 0) => r,
                    (1, 1) => b,
                    _ => g,
                };
                px.extend(v.to_le_bytes());
            }
        }
        enum V { S(u16), L(u32), B(Vec<u8>), Sr(Vec<i32>), R(Vec<u32>), A(Vec<u8>) }
        let strip_off_placeholder = 0u32;
        let mut tags: Vec<(u16, V)> = vec![
            (254, V::L(0)), (256, V::L(w)), (257, V::L(h)), (258, V::S(16)), (259, V::S(1)), (262, V::S(32803)),
            (273, V::L(strip_off_placeholder)), (277, V::S(1)), (278, V::L(h)), (279, V::L(px.len() as u32)), (284, V::S(1)),
            (33421, V::B(vec![2, 0, 2, 0])), (33422, V::B(vec![0, 1, 1, 2])),
            (50706, V::B(vec![1, 4, 0, 0])), (50707, V::B(vec![1, 1, 0, 0])), (50708, V::A(b"PV Test Camera\0".to_vec())),
            (50717, V::L(16383)),
            (50721, V::Sr(vec![1, 1, 0, 1, 0, 1, 0, 1, 1, 1, 0, 1, 0, 1, 0, 1, 1, 1])),
            (50728, V::R(vec![1, 1, 1, 1, 1, 1])),
            (50778, V::S(21)),
        ];
        tags.sort_by_key(|t| t.0);
        let n = tags.len();
        let ifd_len = 2 + n * 12 + 4;
        let mut extra = vec![];
        let data_base = 8 + ifd_len;
        let mut ifd = vec![];
        ifd.extend((n as u16).to_le_bytes());
        let mut strip_pos = 0usize;
        for (tag, v) in &tags {
            let (typ, cnt, bytes): (u16, u32, Vec<u8>) = match v {
                V::S(x) => (3, 1, x.to_le_bytes().to_vec()),
                V::L(x) => (4, 1, x.to_le_bytes().to_vec()),
                V::B(b) => (1, b.len() as u32, b.clone()),
                V::A(b) => (2, b.len() as u32, b.clone()),
                V::Sr(v) => (10, (v.len() / 2) as u32, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
                V::R(v) => (5, (v.len() / 2) as u32, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            };
            ifd.extend(tag.to_le_bytes());
            ifd.extend(typ.to_le_bytes());
            ifd.extend(cnt.to_le_bytes());
            if *tag == 273 {
                strip_pos = ifd.len();
            }
            if bytes.len() <= 4 {
                let mut b = bytes.clone();
                b.resize(4, 0);
                ifd.extend(b);
            } else {
                ifd.extend(((data_base + extra.len()) as u32).to_le_bytes());
                extra.extend(&bytes);
                if extra.len() % 2 == 1 {
                    extra.push(0);
                }
            }
        }
        ifd.extend(0u32.to_le_bytes());
        let px_off = (data_base + extra.len()) as u32;
        ifd[strip_pos..strip_pos + 4].copy_from_slice(&px_off.to_le_bytes());
        let mut out = b"II*\0".to_vec();
        out.extend(8u32.to_le_bytes());
        out.extend(ifd);
        out.extend(extra);
        out.extend(px);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icc_roundtrip_and_header() {
        for s in [Space::Srgb, Space::AdobeRgb, Space::ProPhoto] {
            let p = icc::profile(s);
            if let Some(d) = std::env::var_os("PV_DUMP") { std::fs::write(std::path::Path::new(&d).join(format!("{s:?}.icc")), &p).unwrap(); }
            assert_eq!(u32::from_be_bytes(p[0..4].try_into().unwrap()) as usize, p.len());
            assert_eq!(&p[36..40], b"acsp");
            let (w, h) = (64u16, 48u16);
            let px: Vec<u8> = (0..w as usize * h as usize * 3).map(|i| (i % 251) as u8).collect();
            let mut out = vec![];
            let mut e = Encoder::new(&mut out, 100);
            e.set_sampling_factor(SamplingFactor::F_1_1);
            e.add_icc_profile(&p).unwrap();
            e.encode(&px, w, h, ColorType::Rgb).unwrap();
            let dir = std::env::temp_dir().join(format!("pv-raw-test-{:?}", s));
            std::fs::write(&dir, &out).unwrap();
            let i = probe_decode(&dir).unwrap();
            assert!(i.has_icc && i.width == 64 && i.height == 48);
            let mut d = jpeg_decoder::Decoder::new(&out[..]);
            d.read_info().unwrap();
            assert_eq!(d.icc_profile().unwrap(), p);
            let (tw, th, rgb) = thumbnail(&dir, 32).unwrap();
            assert_eq!(rgb.len(), tw * th * 3);
            std::fs::remove_file(dir).unwrap();
        }
    }

    #[test]
    fn convert_synthetic_dng() {
        let d = std::env::temp_dir().join(format!("pv-dng-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("t.dng");
        std::fs::write(&src, testdng::make(256, 192)).unwrap();
        for (mode, want) in [(ColourMode::Srgb, Space::Srgb), (ColourMode::Adobergb, Space::AdobeRgb), (ColourMode::Prophoto, Space::ProPhoto)] {
            let out = d.join(format!("{want:?}.jpg"));
            let c = convert(&src, &out, mode, 3, &Render::default()).unwrap();
            assert_eq!(c.space, want);
            let i = probe_decode(&out).unwrap();
            assert!(i.has_icc && i.width == c.width && i.height == c.height && c.width >= 200);
            let (w, h, rgb) = thumbnail(&out, 4096).unwrap();
            let px = |x: usize, y: usize| &rgb[(y * w + x) * 3..(y * w + x) * 3 + 3];
            let l = px(w / 4, h / 2);
            let r = px(3 * w / 4, h / 2);
            assert!(l[0] > l[2] + 30, "left should be red-dominant {l:?}");
            assert!(r[2] > r[0] + 30, "right should be blue-dominant {r:?}");
            let mut dd = jpeg_decoder::Decoder::new(std::io::BufReader::new(std::fs::File::open(&out).unwrap()));
            dd.read_info().unwrap();
            assert!(dd.exif_data().is_some());
            assert_eq!(c.hash, *blake3::hash(&std::fs::read(&out).unwrap()).as_bytes());
        }
        assert!(convert(&src, &d.join("Srgb.jpg"), ColourMode::Srgb, 3, &Render::default()).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn render_settings_change_output() {
        let d = std::env::temp_dir().join(format!("pv-render-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("t.dng");
        std::fs::write(&src, testdng::make(256, 192)).unwrap();
        let mean = |name: &str, r: Render| {
            let out = d.join(name);
            let c = convert(&src, &out, ColourMode::Srgb, 3, &r).unwrap();
            let (_, _, rgb) = thumbnail(&out, 4096).unwrap();
            (rgb.iter().map(|&v| v as f64).sum::<f64>() / rgb.len() as f64, c.size)
        };
        let (base, base_size) = mean("base.jpg", Render::default());
        let (bright, _) = mean("bright.jpg", Render { brightness: 2.0, ..Default::default() });
        let (dark, _) = mean("dark.jpg", Render { exposure_ev: -1.0, ..Default::default() });
        let (_, small) = mean("q60.jpg", Render { quality: 60, ..Default::default() });
        assert!(bright > base + 10.0, "brightness: {bright} vs {base}");
        assert!(dark < base - 10.0, "exposure: {dark} vs {base}");
        assert!(small < base_size, "quality: {small} vs {base_size}");
        for r in [Render { highlight: 5, ..Default::default() }, Render { white_balance: WhiteBalance::Auto, auto_bright: true, noise_threshold: 300.0, fbdd_noise: 2, median_passes: 3, ..Default::default() }] {
            let _ = std::fs::remove_file(d.join("x.jpg"));
            mean("x.jpg", r);
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn exif_roundtrip() {
        use exif::experimental::Writer;
        use exif::{Field, In, Tag, Value};
        let fs = [
            Field { tag: Tag::Make, ifd_num: In::PRIMARY, value: Value::Ascii(vec![b"SONY".to_vec()]) },
            Field { tag: Tag::Model, ifd_num: In::PRIMARY, value: Value::Ascii(vec![b"ILCE-7M4".to_vec()]) },
            Field { tag: Tag::Orientation, ifd_num: In::PRIMARY, value: Value::Short(vec![6]) },
            Field { tag: Tag::ExposureTime, ifd_num: In::PRIMARY, value: Value::Rational(vec![exif::Rational { num: 1, denom: 250 }]) },
            Field { tag: Tag::DateTimeOriginal, ifd_num: In::PRIMARY, value: Value::Ascii(vec![b"2020:01:02 03:04:05".to_vec()]) },
        ];
        let mut w = Writer::new();
        fs.iter().for_each(|f| w.push_field(f));
        let mut c = std::io::Cursor::new(vec![]);
        w.write(&mut c, true).unwrap();
        let tiff = c.into_inner();
        let out = exifcopy::build(&tiff, 100, 80, Space::AdobeRgb).expect("built");
        let r = exif::Reader::new().read_raw(out).unwrap();
        let g = |t| r.get_field(t, In::PRIMARY).map(|f| f.display_value().to_string());
        assert_eq!(g(Tag::Make).unwrap(), "\"SONY\"");
        assert_eq!(g(Tag::Orientation).unwrap(), "row 0 at top and column 0 at left");
        assert!(g(Tag::DateTimeOriginal).unwrap().contains("2020-01-02"));
        assert!(g(Tag::ExposureTime).unwrap().contains("1/250"));
        assert_eq!(g(Tag::PixelXDimension).unwrap(), "100");
        assert_eq!(g(Tag::ColorSpace).unwrap(), "uncalibrated");
    }
}

/// Write via temp file + rename; refuses to overwrite.
pub fn write_atomic(dst: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    if dst.exists() {
        bail!("{} exists", dst.display());
    }
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)?;
    }
    let n = dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dst.with_file_name(format!(".{n}.tmp"));
    let r = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}
