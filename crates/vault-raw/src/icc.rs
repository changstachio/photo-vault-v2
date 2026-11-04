use crate::Space;

fn s15(v: f64) -> [u8; 4] {
    ((v * 65536.0).round() as i32).to_be_bytes()
}

fn xyz(v: [f64; 3]) -> Vec<u8> {
    let mut d = b"XYZ \0\0\0\0".to_vec();
    for c in v {
        d.extend(s15(c));
    }
    d
}

fn text(sig: &[u8; 4], s: &str) -> Vec<u8> {
    let mut d = sig.to_vec();
    d.extend([0; 4]);
    d.extend(s.as_bytes());
    d.push(0);
    d
}

fn desc(s: &str) -> Vec<u8> {
    let mut d = b"desc\0\0\0\0".to_vec();
    d.extend((s.len() as u32 + 1).to_be_bytes());
    d.extend(s.as_bytes());
    d.push(0);
    d.extend([0u8; 8]);
    d.extend([0u8; 3]);
    d.extend([0u8; 67]);
    d
}

fn curve(space: Space) -> Vec<u8> {
    let mut d = b"curv\0\0\0\0".to_vec();
    match space {
        Space::Srgb => {
            d.extend(1024u32.to_be_bytes());
            for i in 0..1024 {
                let v = i as f64 / 1023.0;
                let l = if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
                d.extend(((l * 65535.0).round() as u16).to_be_bytes());
            }
        }
        Space::AdobeRgb => {
            d.extend(1u32.to_be_bytes());
            d.extend(563u16.to_be_bytes());
        }
        Space::ProPhoto => {
            d.extend(1u32.to_be_bytes());
            d.extend(461u16.to_be_bytes());
        }
    }
    d
}

pub fn profile(space: Space) -> Vec<u8> {
    let (name, r, g, b) = match space {
        Space::Srgb => ("sRGB IEC61966-2.1 (PhotoVault)", [0.4360747, 0.2225045, 0.0139322], [0.3850649, 0.7168786, 0.0971045], [0.1430804, 0.0606169, 0.7141733]),
        Space::AdobeRgb => ("Adobe RGB (1998) compatible (PhotoVault)", [0.6097559, 0.3111242, 0.0194811], [0.2052401, 0.6256560, 0.0608902], [0.1492240, 0.0632197, 0.7448387]),
        Space::ProPhoto => ("ProPhoto RGB (PhotoVault)", [0.7977, 0.2880, 0.0000], [0.1352, 0.7119, 0.0000], [0.0313, 0.0001, 0.8249]),
    };
    let trc = curve(space);
    let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"desc", desc(name)),
        (b"cprt", text(b"text", "Public domain")),
        (b"wtpt", xyz([0.9642, 1.0, 0.8249])),
        (b"rXYZ", xyz(r)),
        (b"gXYZ", xyz(g)),
        (b"bXYZ", xyz(b)),
        (b"rTRC", trc.clone()),
        (b"gTRC", trc.clone()),
        (b"bTRC", trc),
    ];
    let table = 4 + tags.len() * 12;
    let mut off = 128 + table;
    let mut entries = vec![];
    let mut body = vec![];
    for (sig, data) in &tags {
        entries.extend(*sig);
        entries.extend((off as u32).to_be_bytes());
        entries.extend((data.len() as u32).to_be_bytes());
        let pad = (4 - data.len() % 4) % 4;
        body.extend(data);
        body.extend(vec![0u8; pad]);
        off += data.len() + pad;
    }
    let mut p = vec![0u8; 128];
    p[0..4].copy_from_slice(&(off as u32).to_be_bytes());
    p[8..12].copy_from_slice(&[2, 0x40, 0, 0]);
    p[12..16].copy_from_slice(b"mntr");
    p[16..20].copy_from_slice(b"RGB ");
    p[20..24].copy_from_slice(b"XYZ ");
    p[24..30].copy_from_slice(&[0x07, 0xE8, 0, 1, 0, 1]);
    p[36..40].copy_from_slice(b"acsp");
    p[68..72].copy_from_slice(&s15(0.9642));
    p[72..76].copy_from_slice(&s15(1.0));
    p[76..80].copy_from_slice(&s15(0.8249));
    p.extend((tags.len() as u32).to_be_bytes());
    p.extend(entries);
    p.extend(body);
    p
}
