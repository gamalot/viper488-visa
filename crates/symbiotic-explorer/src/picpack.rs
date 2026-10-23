//! Picture packs: a zip of pictures for the pictures window to take in, so that a user need not find and trace a
//! picture of every instrument. The program carries none; a pack is made, and handed on, apart from it.
//!
//! In a pack: `pictures.conf` (the rules, as the pictures folder has them), and for each rule's `NAME.png` the image it
//! is made of, `NAME.<ext>`, and what was done to it, `NAME.json` (zoom, position, height, line art, threshold). A line
//! drawing's image is a one-bit grey PNG (white 1, black 0), at the size it came. The window makes each picture again
//! from these (the editor's own drawing), so a pack holds no picture of its own.

use serde::{Deserialize, Serialize};
use symbiotic_core::pictures;

use crate::blocking;

/// The largest pack taken in, and the most its files may come to unpacked, all together.
const MAX_PACK: usize = 256 * 1024 * 1024;

/// One picture of a pack being made: its rule, and its image, either as grey levels to be made a one-bit PNG (a line drawing)
/// or as it came (a photograph).
#[derive(Deserialize)]
pub struct PackItem {
    pattern: String,
    dark_keep: bool,
    /// The picture's file name in the rules (`NAME.png`).
    file: String,
    /// What was done to the image (JSON).
    edit: String,
    /// A line drawing: its width and height, and one byte a pixel, 0 for ink and anything else for paper, in base64.
    gray: Option<Gray>,
    /// Otherwise the image as it came, a data URL, and its kind.
    original: Option<String>,
    ext: Option<String>,
}

#[derive(Deserialize)]
pub struct Gray {
    width: u32,
    height: u32,
    base64: String,
}

/// One picture of a pack read: its rule, its image as a data URL and what was done to it.
#[derive(Serialize)]
pub struct PackEntry {
    pattern: String,
    dark_keep: bool,
    original: String,
    ext: String,
    edit: Option<String>,
}

/// One bit a pixel, ink black (0), paper white (1), grey: what a line drawing is.
pub fn one_bit_png(width: u32, height: u32, gray: &[u8]) -> Result<Vec<u8>, String> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || gray.len() != w * h {
        return Err(format!("{} grey levels are not a {width} by {height} image", gray.len()));
    }
    let row = w.div_ceil(8);
    let mut bits = vec![0u8; row * h];
    for y in 0..h {
        for x in 0..w {
            if gray[y * w + x] != 0 {
                bits[y * row + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, width, height);
    e.set_color(png::ColorType::Grayscale);
    e.set_depth(png::BitDepth::One);
    e.set_compression(png::Compression::High);
    let mut w = e.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(&bits).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

fn stem(file: &str) -> &str {
    file.rsplit_once('.').map_or(file, |(s, _)| s)
}

/// Make a pack of `items` where the user says in the save dialog (starting in the Downloads folder); its path, or None when
/// the user cancels.
#[tauri::command]
pub async fn export_picture_pack(window: tauri::WebviewWindow, items: Vec<PackItem>, kind: String) -> Result<Option<String>, String> {
    blocking(move || {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut rules = String::from("# A picture pack: the rules, and beside each picture's name the image it is made of and what was done to it.\n");
        for it in items {
            if !pictures::valid_file_name(&it.file) {
                return Err(format!("{:?} is not a picture's name", it.file));
            }
            let s = stem(&it.file).to_string();
            let (ext, bytes) = match (it.gray, it.original, it.ext) {
                (Some(g), _, _) => {
                    let gray = pictures::base64_decode(&g.base64).ok_or("the grey levels are not base64")?;
                    ("png".to_string(), one_bit_png(g.width, g.height, &gray)?)
                }
                (None, Some(o), Some(ext)) => {
                    let ext = ext.to_ascii_lowercase();
                    if !pictures::ORIGINAL_EXTS.contains(&ext.as_str()) {
                        return Err(format!("{ext:?} is not a kind of image a pack holds"));
                    }
                    let body = o.split_once(',').map_or(o.as_str(), |(_, b)| b);
                    (ext, pictures::base64_decode(body).ok_or("the image is not base64")?)
                }
                _ => return Err(format!("{}: no image", it.pattern)),
            };
            rules.push_str(&pictures::format_rule(&pictures::Rule { pattern: it.pattern, file: it.file.clone(), dark_keep: it.dark_keep }));
            rules.push('\n');
            files.push((format!("{s}.{ext}"), bytes));
            files.push((format!("{s}.json"), it.edit.into_bytes()));
        }
        files.insert(0, ("pictures.conf".to_string(), rules.into_bytes()));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let local = (now as i64 + symbiotic_core::iotrace::local_offset_s()).max(0) as u64;
        let Some(path) = crate::save_dialog(&window, &format!("symbiotic-pictures-{}.zip", stamp(local)), &kind, "zip")? else { return Ok(None) };
        symbiotic_core::conffile::write(&path, symbiotic_core::diag::zip(&files, local)).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Some(path.display().to_string()))
    })
    .await
}

/// `YYYYMMDD-HHMMSS` of seconds since 1970 (local time already).
fn stamp(t: u64) -> String {
    let (d, s) = ((t / 86400) as i64, t % 86400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = d + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// The files of a zip: stored or deflated, read from its central directory.
pub fn unzip(z: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let u16_at = |i: usize| z.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |i: usize| z.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let bad = || "not a zip file, or a damaged one".to_string();
    // The end of central directory record: the last one, with no more than a 64 KiB comment after it.
    let eocd = (0..z.len().saturating_sub(21)).rev().take(65_557).find(|&i| u32_at(i) == Some(0x0605_4b50)).ok_or_else(bad)?;
    let count = u16_at(eocd + 10).ok_or_else(bad)? as usize;
    let mut at = u32_at(eocd + 16).ok_or_else(bad)? as usize;
    let mut out = Vec::new();
    let mut total = 0usize;
    for _ in 0..count {
        if u32_at(at) != Some(0x0201_4b50) {
            return Err(bad());
        }
        let method = u16_at(at + 10).ok_or_else(bad)?;
        let (packed, size) = (u32_at(at + 20).ok_or_else(bad)? as usize, u32_at(at + 24).ok_or_else(bad)? as usize);
        let (nlen, xlen, clen) = (u16_at(at + 28).ok_or_else(bad)? as usize, u16_at(at + 30).ok_or_else(bad)? as usize, u16_at(at + 32).ok_or_else(bad)? as usize);
        let local = u32_at(at + 42).ok_or_else(bad)? as usize;
        let name = String::from_utf8_lossy(z.get(at + 46..at + 46 + nlen).ok_or_else(bad)?).into_owned();
        at += 46 + nlen + xlen + clen;
        if u32_at(local) != Some(0x0403_4b50) {
            return Err(bad());
        }
        let start = local + 30 + u16_at(local + 26).ok_or_else(bad)? as usize + u16_at(local + 28).ok_or_else(bad)? as usize;
        let data = z.get(start..start + packed).ok_or_else(bad)?;
        if name.ends_with('/') {
            continue;
        }
        // Each file's size is checked against what is left of the limit before it is unpacked: many files
        // that each stay under it could otherwise come to any amount.
        if size > MAX_PACK - total {
            return Err(format!("the pack's files come to more than {} MiB unpacked", MAX_PACK >> 20));
        }
        total += size;
        let data = match method {
            0 => data.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, size).map_err(|_| format!("{name}: damaged"))?,
            m => return Err(format!("{name}: compressed in a way Symbiotic Explorer does not read ({m})")),
        };
        if data.len() != size {
            return Err(format!("{name}: damaged"));
        }
        out.push((name, data));
    }
    Ok(out)
}

/// The pictures of a pack (the whole file, base64): each rule with its image and what was done to it, for the window to make the
/// pictures of. A rule whose image is not in the pack is left out; a file in a folder of the zip counts by its own name.
#[tauri::command]
pub async fn read_picture_pack(base64: String) -> Result<Vec<PackEntry>, String> {
    blocking(move || {
        let body = base64.split_once(',').map_or(base64.as_str(), |(_, b)| b);
        let zip = pictures::base64_decode(body).ok_or("the pack is not base64")?;
        if zip.len() > MAX_PACK {
            return Err("the pack is more than 256 MiB".into());
        }
        let files = unzip(&zip)?;
        let base = |n: &str| n.rsplit('/').next().unwrap_or(n).to_string();
        let find = |name: &str| files.iter().find(|(n, _)| base(n).eq_ignore_ascii_case(name)).map(|(_, d)| d);
        let conf = find("pictures.conf").ok_or("no pictures.conf in the pack: it is not a picture pack")?;
        let mut out = Vec::new();
        for r in pictures::parse(&String::from_utf8_lossy(conf)) {
            let s = stem(&r.file);
            let Some((ext, bytes)) = pictures::ORIGINAL_EXTS.iter().find_map(|e| find(&format!("{s}.{e}")).map(|b| (*e, b))) else { continue };
            let mime = match ext {
                "jpg" | "jpeg" => "image/jpeg".to_string(),
                other => format!("image/{other}"),
            };
            out.push(PackEntry {
                pattern: r.pattern,
                dark_keep: r.dark_keep,
                original: format!("data:{mime};base64,{}", pictures::base64_encode(bytes)),
                ext: ext.to_string(),
                edit: find(&format!("{s}.json")).map(|b| String::from_utf8_lossy(b).into_owned()),
            });
        }
        Ok(out)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_bit_png_and_a_pack_round_trip() {
        // 10 by 2: ink at (0,0) and (9,1), the rest paper.
        let mut g = vec![255u8; 20];
        g[0] = 0;
        g[19] = 0;
        let png = one_bit_png(10, 2, &g).unwrap();
        let mut d = png::Decoder::new(std::io::Cursor::new(&png)).read_info().unwrap();
        let info = d.info();
        assert_eq!((info.width, info.height, info.bit_depth, info.color_type), (10, 2, png::BitDepth::One, png::ColorType::Grayscale));
        let mut buf = vec![0; d.output_buffer_size().unwrap()];
        d.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..4], &[0b0111_1111, 0b1100_0000, 0b1111_1111, 0b1000_0000]);
        assert!(one_bit_png(10, 3, &g).is_err());

        let files = vec![("pictures.conf".to_string(), b"53181 = 53181A.png\n".to_vec()), ("53181A.png".to_string(), png.clone())];
        assert_eq!(unzip(&symbiotic_core::diag::zip(&files, 0)).unwrap(), files);
        assert!(unzip(b"not a zip at all, not at all").is_err());
        // A file that says it unpacks to more than the limit is refused before it is unpacked.
        let mut z = symbiotic_core::diag::zip(&files, 0);
        let cd = z.windows(4).position(|w| w == [0x50, 0x4b, 0x01, 0x02]).unwrap();
        z[cd + 24..cd + 28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(unzip(&z).unwrap_err().contains("unpacked"));
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_760_500_000), "20251015-034640");
    }
}
