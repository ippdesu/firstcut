//! XMP 侧车写出（M4/M7）
//!
//! 为每张照片写侧车 `<stem>.xmp`（Lightroom / Camera Raw / darktable 均可读）：
//! - `xmp:Rating`（1-5 星）
//! - `firstcut:` 自定义命名空间存 5 维子分 + 人脸数 + 连拍信息
//! - 建议曝光 EV（仅曝光出容差带时）：`crs:Exposure2`（Lightroom 自家开发字段）
//!   + `firstcut:suggestedEV`（信息字段）；实际修正由用户在 Lightroom 里确认
//!
//! 命名说明：Lightroom/ACR 读 `<basename>.xmp`（不带扩展名），darktable 也兼容
//! 该格式（另外还认自己的 `<basename>.<ext>.xmp`）。所以统一用 `<stem>.xmp`，
//! 两家都能读；同一 stem 的 JPG/ARW 共用一个侧车。
//!
//! 保护策略：已存在的侧车若无 firstcut 命名空间（可能是 LR 等其他软件写的），
//! 不覆盖，只警告。

use std::collections::HashMap;

use crate::config::MetricParams;
use crate::scan::PhotoEntry;
use crate::score::PixelScores;

/// 自定义命名空间 URI
pub const NS_FIRSTCUT: &str = "http://firstcut.local/ns/";

/// 总分 → 星级映射（绝对阈值模式，默认 75/60/45/30）
pub fn rating_from_total(total: f64, m: &MetricParams) -> u8 {
    if total >= m.rating_5 {
        5
    } else if total >= m.rating_4 {
        4
    } else if total >= m.rating_3 {
        3
    } else if total >= m.rating_2 {
        2
    } else {
        1
    }
}

/// 批次内相对星级：按总分排名百分位给星
///
/// 为什么需要它：绝对阈值要求"分数的绝对值有跨批次含义"，但各维度为了防止
/// 误杀都带中性地板（清晰度 50 保底、构图无主体 60、曝光容差带），
/// 实测一批 119 张全部落在 4~5 星，星级就失去了筛选作用。
/// 改成批次内相对排名后，每批都保证有区分度；总分仍原样写进 CSV/XMP，
/// 跨批次比较用分数而不是星级。
///
/// 百分位用**并列名次的平均位次**计算：分数相同的照片拿同一个星级，
/// 不会被人为拆开。
pub fn ratings_relative(pairs: &[(String, f64)], m: &MetricParams) -> HashMap<String, u8> {
    let n = pairs.len();
    let mut out = HashMap::with_capacity(n);
    if n == 0 {
        return out;
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| {
        pairs[b].1.partial_cmp(&pairs[a].1).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut i = 0usize;
    while i < n {
        // 找出一段同分（并列）区间 [i, j]
        let mut j = i;
        while j + 1 < n && (pairs[idx[j + 1]].1 - pairs[idx[i]].1).abs() < 1e-9 {
            j += 1;
        }
        let avg_rank = (i + j) as f64 / 2.0;
        let pct = avg_rank / n as f64 * 100.0;
        let star = if pct < m.star_five_pct {
            5
        } else if pct < m.star_four_pct {
            4
        } else if pct < m.star_three_pct {
            3
        } else if pct < m.star_two_pct {
            2
        } else {
            1
        };
        for k in i..=j {
            out.insert(pairs[idx[k]].0.clone(), star);
        }
        i = j + 1;
    }
    out
}

/// 按配置模式计算整批星级
pub fn assign_ratings(pairs: &[(String, f64)], m: &MetricParams) -> HashMap<String, u8> {
    if m.star_mode.eq_ignore_ascii_case("absolute") {
        pairs.iter().map(|(k, t)| (k.clone(), rating_from_total(*t, m))).collect()
    } else {
        ratings_relative(pairs, m)
    }
}

/// 生成 XMP 侧车内容
pub fn render_xmp(e: &PhotoEntry, s: &PixelScores, total: f64, rating: u8,
    suggested_ev: Option<f64>,
) -> String {
    let faces = e.faces.parse::<usize>().unwrap_or(0);
    // 建议曝光 EV（Lightroom 联动）：有建议才写入，无建议侧车完全不变。
    // crs:Exposure2 = LR 自家曝光开发字段（侧车被读取时建议值成为 LR 初始曝光）；
    // firstcut:suggestedEV = 信息性元数据字段（LR 元数据面板可见）。
    let (crs_ns, crs_attr, ev_line) = match suggested_ev {
        Some(ev) => (
            String::from(" xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\""),
            format!(" crs:Exposure2=\"{ev:.2}\""),
            format!("    <firstcut:suggestedEV>{ev:+.2}</firstcut:suggestedEV>\n"),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    let (crs_ns, crs_attr, ev_line) = (crs_ns.as_str(), crs_attr.as_str(), ev_line.as_str());
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         \x20<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         \x20\x20<rdf:Description rdf:about=\"\"\n\
         \x20\x20\x20 xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n\
         \x20\x20\x20 xmlns:firstcut=\"{NS_FIRSTCUT}\"{crs_ns}{crs_attr}>\n\
         \x20\x20\x20\x20<xmp:Rating>{rating}</xmp:Rating>\n\
         \x20\x20\x20\x20<firstcut:sharpness>{:.1}</firstcut:sharpness>\n\
         \x20\x20\x20\x20<firstcut:exposure>{:.1}</firstcut:exposure>\n\
         \x20\x20\x20\x20<firstcut:noise>{:.1}</firstcut:noise>\n\
         \x20\x20\x20\x20<firstcut:composition>{:.1}</firstcut:composition>\n\
         \x20\x20\x20\x20<firstcut:aesthetic>{:.1}</firstcut:aesthetic>\n\
         \x20\x20\x20\x20<firstcut:faces>{faces}</firstcut:faces>\n\
         \x20\x20\x20\x20<firstcut:total>{total:.1}</firstcut:total>\n\
         {ev_line}\x20\x20\x20\x20<firstcut:burstGroup>{}</firstcut:burstGroup>\n\
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
pub fn write_sidecar(
    e: &PhotoEntry,
    s: &PixelScores,
    total: f64,
    rating: u8,
    suggested_ev: Option<f64>,
) -> anyhow::Result<bool> {
    if e.total_score.is_empty() {
        return Ok(false);
    }
    let path = std::path::Path::new(&e.path);
    // 侧车命名：<stem>.xmp（如 DSC00001.xmp）—— Lightroom/ACR 的约定，
    // darktable 也读这种格式。同一 stem 的 JPG/ARW 共用一个侧车。
    // 必须保留原文件名大小写（大小写敏感的文件系统上小写名会被视为不存在）。
    let stem = crate::scan::stem_raw_of(&e.filename);
    let sidecar = path.with_file_name(format!("{stem}.xmp"));

    if sidecar.exists() {
        if let Ok(content) = std::fs::read_to_string(&sidecar) {
            if !content.contains(NS_FIRSTCUT) {
                eprintln!("[xmp] 跳过 {}（侧车为其他软件所写，未覆盖）", sidecar.display());
                return Ok(false);
            }
        }
    }

    let xml = render_xmp(e, s, total, rating, suggested_ev);
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
            pair_id: "testpic/jpg|dsc00001".into(),
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
            suggested_ev: String::new(),
            stars: "4".into(),
            faces: "1".into(),
            analysis_ok: "true".into(),
            burst_group: "3".into(),
            burst_size: "2".into(),
            burst_rank: "1".into(),
            burst_keep: "true".into(),
            burst_pose: "1".into(),
        }
    }

    #[test]
    fn rating_mapping() {
        let m = MetricParams::default(); // 75/60/45/30
        assert_eq!(rating_from_total(90.0, &m), 5);
        assert_eq!(rating_from_total(75.0, &m), 5);
        assert_eq!(rating_from_total(74.9, &m), 4);
        assert_eq!(rating_from_total(60.0, &m), 4);
        assert_eq!(rating_from_total(59.9, &m), 3);
        assert_eq!(rating_from_total(45.0, &m), 3);
        assert_eq!(rating_from_total(44.9, &m), 2);
        assert_eq!(rating_from_total(30.0, &m), 2);
        assert_eq!(rating_from_total(29.9, &m), 1);
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
        let xml = render_xmp(&e, &s, 66.0, 4, None);
        assert!(xml.contains("<xmp:Rating>4</xmp:Rating>"));
        assert!(xml.contains("<firstcut:sharpness>75.0</firstcut:sharpness>"));
        assert!(xml.contains("<firstcut:aesthetic>45.0</firstcut:aesthetic>"));
        assert!(xml.contains("<firstcut:burstKeep>true</firstcut:burstKeep>"));
        assert!(xml.starts_with("<?xpacket"));
        assert!(xml.contains("xmlns:firstcut=\"http://firstcut.local/ns/\""));
    }

    /// 批次内相对分档：10/30/65/90 百分位对应 5/4/3/2 星
    #[test]
    fn relative_ratings_follow_percentiles() {
        let m = MetricParams::default();
        // 10 张，分数 100..91（严格递减，无并列）
        let pairs: Vec<(String, f64)> =
            (0..10).map(|i| (format!("p{i}"), 100.0 - i as f64)).collect();
        let r = ratings_relative(&pairs, &m);
        // 位次 0（前 0~10%）→ 5 星；位次 1,2（10~30%）→ 4 星；3..6（30~65%）→ 3 星；
        // 7,8（65~90%）→ 2 星；位次 9（90~100%）→ 1 星
        assert_eq!(r["p0"], 5);
        assert_eq!(r["p1"], 4);
        assert_eq!(r["p2"], 4);
        assert_eq!(r["p3"], 3);
        assert_eq!(r["p6"], 3);
        assert_eq!(r["p7"], 2);
        assert_eq!(r["p8"], 2);
        assert_eq!(r["p9"], 1);
    }

    /// 同分并列取平均位次，不会被人为拆成不同星级
    #[test]
    fn relative_ratings_tie_share_star() {
        let m = MetricParams::default();
        // 10 张全部同分 → 全部同一星级
        let pairs: Vec<(String, f64)> =
            (0..10).map(|i| (format!("p{i}"), 70.0)).collect();
        let r = ratings_relative(&pairs, &m);
        let first = r["p0"];
        assert!(r.values().all(|&v| v == first), "同分应同星: {r:?}");
    }

    /// absolute 模式仍走总分阈值
    #[test]
    fn absolute_mode_uses_thresholds() {
        let m = MetricParams { star_mode: "absolute".into(), ..MetricParams::default() };
        let pairs = vec![("a".to_string(), 80.0), ("b".to_string(), 20.0)];
        let r = assign_ratings(&pairs, &m);
        assert_eq!(r["a"], 5);
        assert_eq!(r["b"], 1);
    }

    /// 建议 EV 存在：写 crs:Exposure2（LR 属性）+ firstcut:suggestedEV（元数据），且位置正确
    #[test]
    fn xmp_contains_suggested_ev_when_present() {
        let e = entry();
        let s = PixelScores {
            sharpness: 75.0,
            exposure: 30.0,
            noise: 60.0,
            composition: 60.0,
            aesthetic: 45.0,
        };
        let xml = render_xmp(&e, &s, 55.0, 2, Some(-0.33));
        assert!(xml.contains(r#"<firstcut:suggestedEV>-0.33</firstcut:suggestedEV>"#));
        assert!(xml.contains(r#"crs:Exposure2="-0.33""#));
        assert!(xml.contains("xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\""));
        // 建议行位于 total 与 burstGroup 之间
        let i = xml.find("<firstcut:total>").unwrap();
        let j = xml.find("<firstcut:suggestedEV>").unwrap();
        let k = xml.find("<firstcut:burstGroup>").unwrap();
        assert!(i < j && j < k, "suggestedEV 应在 total 之后、burstGroup 之前");
    }

    /// 无建议：侧车与旧版逐字节一致（无 crs 命名空间、无 suggestedEV、无 Exposure2）
    #[test]
    fn xmp_untouched_when_no_suggested_ev() {
        let e = entry();
        let s = PixelScores {
            sharpness: 75.0,
            exposure: 80.0,
            noise: 60.0,
            composition: 60.0,
            aesthetic: 45.0,
        };
        let xml_none = render_xmp(&e, &s, 66.0, 4, None);
        assert!(!xml_none.contains("suggestedEV"), "无建议不应出现 suggestedEV");
        assert!(!xml_none.contains("Exposure2"), "无建议不应出现 Exposure2");
        assert!(!xml_none.contains("crs"), "无建议不应出现 crs 命名空间");
        let xml_some = render_xmp(&e, &s, 66.0, 4, Some(1.0));
        assert!(xml_some.contains(r#"<firstcut:suggestedEV>+1.00</firstcut:suggestedEV>"#));
        assert!(xml_some.contains(r#"crs:Exposure2="1.00""#));
    }
}
