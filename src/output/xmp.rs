//! XMP 侧车写出（M4）
//!
//! 为每张照片写同名 `.xmp` 侧车：
//! - `xmp:Rating`（1-5 星，由总分映射，Lightroom/Capture One 可读）
//! - `firstcut:` 自定义命名空间存 5 维子分 + 人脸数 + 连拍信息
//!
//! 保护策略：已存在的侧车若无 firstcut 命名空间（可能是 LR 等其他软件写的），
//! 不覆盖，只警告。

use crate::scan::PhotoEntry;
use crate::score::PixelScores;

/// 自定义命名空间 URI
pub const NS_FIRSTCUT: &str = "http://firstcut.local/ns/";

/// 总分 → 星级映射（M5 可校准）
pub fn rating_from_total(total: f64) -> u8 {
    if total >= 80.0 {
        5
    } else if total >= 65.0 {
        4
    } else if total >= 50.0 {
        3
    } else if total >= 35.0 {
        2
    } else {
        1
    }
}

/// 生成 XMP 侧车内容
pub fn render_xmp(e: &PhotoEntry, s: &PixelScores, total: f64) -> String {
    let rating = rating_from_total(total);
    let faces = e.faces.parse::<usize>().unwrap_or(0);
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         \x20<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         \x20\x20<rdf:Description rdf:about=\"\"\n\
         \x20\x20\x20 xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n\
         \x20\x20\x20 xmlns:firstcut=\"{NS_FIRSTCUT}\">\n\
         \x20\x20\x20\x20<xmp:Rating>{rating}</xmp:Rating>\n\
         \x20\x20\x20\x20<firstcut:sharpness>{:.1}</firstcut:sharpness>\n\
         \x20\x20\x20\x20<firstcut:exposure>{:.1}</firstcut:exposure>\n\
         \x20\x20\x20\x20<firstcut:noise>{:.1}</firstcut:noise>\n\
         \x20\x20\x20\x20<firstcut:composition>{:.1}</firstcut:composition>\n\
         \x20\x20\x20\x20<firstcut:aesthetic>{:.1}</firstcut:aesthetic>\n\
         \x20\x20\x20\x20<firstcut:faces>{faces}</firstcut:faces>\n\
         \x20\x20\x20\x20<firstcut:total>{total:.1}</firstcut:total>\n\
         \x20\x20\x20\x20<firstcut:burstGroup>{}</firstcut:burstGroup>\n\
         \x20\x20\x20\x20<firstcut:burstRank>{}</firstcut:burstRank>\n\
         \x20\x20\x20\x20<firstcut:burstKeep>{}</firstcut:burstKeep>\n\
         \x20\x20</rdf:Description>\n\
         \x20</rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>",
        s.sharpness,
        s.exposure,
        s.noise,
        s.composition,
        s.aesthetic,
        e.burst_group,
        e.burst_rank,
        e.burst_keep,
    )
}

/// 写出侧车；他人侧车（无 firstcut 命名空间）不覆盖。
/// 返回 Ok(true) 表示已写入，Ok(false) 表示跳过（他人侧车/无分数）。
pub fn write_sidecar(e: &PhotoEntry, s: &PixelScores, total: f64) -> anyhow::Result<bool> {
    if e.total_score.is_empty() {
        return Ok(false);
    }
    let path = std::path::Path::new(&e.path);
    // Lightroom 侧车命名约定：<stem>.<原扩展名>.xmp（如 DSC00001.ARW.xmp）
    let stem = crate::scan::stem_of(&e.filename);
    let ext = crate::scan::extension_of(&e.filename);
    let sidecar = path.with_file_name(format!("{stem}.{ext}.xmp"));

    if sidecar.exists() {
        if let Ok(content) = std::fs::read_to_string(&sidecar) {
            if !content.contains(NS_FIRSTCUT) {
                eprintln!("[xmp] 跳过 {}（侧车为其他软件所写，未覆盖）", sidecar.display());
                return Ok(false);
            }
        }
    }

    let xml = render_xmp(e, s, total);
    std::fs::write(&sidecar, xml)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> PhotoEntry {
        PhotoEntry {
            path: "testpic/JPG/DSC00001.JPG".into(),
            filename: "DSC00001.JPG".into(),
            extension: "jpg".into(),
            is_raw: false,
            has_pair: false,
            date_time_original: String::new(),
            camera_make: String::new(),
            camera_model: String::new(),
            lens_model: String::new(),
            iso: "100".into(),
            f_number: String::new(),
            shutter_speed: String::new(),
            focal_length: String::new(),
            sharpness_score: "75.0".into(),
            exposure_score: "80.0".into(),
            noise_score: "60.0".into(),
            composition_score: "60.0".into(),
            aesthetic_score: "45.0".into(),
            total_score: "66.0".into(),
            faces: "1".into(),
            burst_group: "3".into(),
            burst_size: "2".into(),
            burst_rank: "1".into(),
            burst_keep: "true".into(),
        }
    }

    #[test]
    fn rating_mapping() {
        assert_eq!(rating_from_total(90.0), 5);
        assert_eq!(rating_from_total(80.0), 5);
        assert_eq!(rating_from_total(79.9), 4);
        assert_eq!(rating_from_total(65.0), 4);
        assert_eq!(rating_from_total(64.9), 3);
        assert_eq!(rating_from_total(50.0), 3);
        assert_eq!(rating_from_total(49.9), 2);
        assert_eq!(rating_from_total(35.0), 2);
        assert_eq!(rating_from_total(34.9), 1);
    }

    #[test]
    fn xmp_contains_key_fields() {
        let e = entry();
        let s = PixelScores {
            sharpness: 75.0,
            exposure: 80.0,
            noise: 60.0,
            composition: 60.0,
            aesthetic: 45.0,
        };
        let xml = render_xmp(&e, &s, 66.0);
        assert!(xml.contains("<xmp:Rating>4</xmp:Rating>"), "66 分应为 4 星");
        assert!(xml.contains("<firstcut:sharpness>75.0</firstcut:sharpness>"));
        assert!(xml.contains("<firstcut:aesthetic>45.0</firstcut:aesthetic>"));
        assert!(xml.contains("<firstcut:burstKeep>true</firstcut:burstKeep>"));
        assert!(xml.starts_with("<?xpacket"));
        assert!(xml.contains("xmlns:firstcut=\"http://firstcut.local/ns/\""));
    }
}
