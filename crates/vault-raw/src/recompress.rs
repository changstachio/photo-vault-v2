use anyhow::{bail, Context, Result};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

use crate::Chroma;

/// Metadata segments (EXIF, XMP, ICC, IPTC...) of a JPG, in file order: (APPn number, payload).
fn app_segments(j: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    if j.len() < 4 || j[0] != 0xFF || j[1] != 0xD8 {
        return out;
    }
    let mut i = 2;
    while i + 4 <= j.len() && j[i] == 0xFF {
        let m = j[i + 1];
        if m == 0xDA || m == 0xD9 {
            break;
        }
        if m == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([j[i + 2], j[i + 3]]) as usize;
        if len < 2 || i + 2 + len > j.len() {
            break;
        }
        if (0xE1..=0xEF).contains(&m) {
            out.push((m - 0xE0, j[i + 4..i + 2 + len].to_vec()));
        }
        i += 2 + len;
    }
    out
}

/// Re-encode a JPG at `quality`/`chroma`, keeping EXIF, XMP, ICC and IPTC segments unchanged.
/// Returns the new bytes and the pixel size. The caller decides whether the result is worth keeping.
pub fn recompress(jpg: &[u8], quality: u8, chroma: Chroma) -> Result<(Vec<u8>, u16, u16)> {
    let mut d = jpeg_decoder::Decoder::new(jpg);
    let px = d.decode().map_err(|e| anyhow::anyhow!("{e}"))?;
    let info = d.info().context("no image info")?;
    let out = recompress_pixels(jpg, &px, &info, quality, chroma)?;
    Ok((out, info.width, info.height))
}

/// Encode already-decoded pixels of `jpg`, copying its metadata segments.
pub fn recompress_pixels(jpg: &[u8], px: &[u8], info: &jpeg_decoder::ImageInfo, quality: u8, chroma: Chroma) -> Result<Vec<u8>> {
    let color = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => ColorType::Rgb,
        jpeg_decoder::PixelFormat::L8 => ColorType::Luma,
        other => bail!("unsupported JPG pixel format {other:?}"),
    };
    let mut out = Vec::with_capacity(jpg.len());
    let mut enc = Encoder::new(&mut out, quality.clamp(1, 100));
    enc.set_sampling_factor(match chroma {
        Chroma::C444 => SamplingFactor::R_4_4_4,
        Chroma::C422 => SamplingFactor::R_4_2_2,
        Chroma::C420 => SamplingFactor::R_4_2_0,
    });
    enc.set_optimized_huffman_tables(true);
    for (n, data) in app_segments(jpg) {
        enc.add_app_segment(n, data).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    enc.encode(px, info.width, info.height, color).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(out)
}
