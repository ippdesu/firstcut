//! XMP 侧车写出（M4/M7）
//!
//! 为每张照片写侧车 `<stem>.xmp`（Lightroom / Camera Raw / darktable 均可读）：
//! - `xmp:Rating`（1-5 星）
//! - `firstcut:` 自定义命名空间存 5 维子分 + 人脸数 + 连拍信息
//! - 建议曝光 EV（仅曝光出容差带时）：`crs:Exposure2012`（Lightroom / Camera Raw 开发字段，
//!   附带 `crs:HasSettings="True"` 与 `crs:ProcessVersion="11.0"`）
//!   + `firstcut:suggestedEV`（信息字段）；实际修正由用户在 Lightroom / ACR 里确认
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
const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";

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
    // 建议曝光 EV（Lightroom / ACR 联动）：有建议才写入，无建议侧车完全不变。
    // crs:Exposure2012 + crs:HasSettings + crs:ProcessVersion = ACR/LR 现代开发引擎字段；
    // firstcut:suggestedEV = 信息性元数据字段（元数据面板可见）。
    let (crs_ns, crs_attr, ev_line) = match suggested_ev {
        Some(ev) => (
            String::from(" xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\""),
            format!(" crs:HasSettings=\"True\" crs:ProcessVersion=\"11.0\" crs:Exposure2012=\"{ev:+.2}\""),
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
    root: &std::path::Path,
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
    let file = crate::storage::FileRef::inside(root, &sidecar)?;

    let xml = if let Some(bytes) = file.read(16 * 1024 * 1024)? {
        let content = String::from_utf8(bytes)?;
        if !content.contains(NS_FIRSTCUT) {
            eprintln!("[xmp] 跳过 {}（侧车为其他软件所写，未覆盖）", sidecar.display());
            return Ok(false);
        }
        merge_xmp(&content, e, s, total, rating, suggested_ev)?
    } else {
        render_xmp(e, s, total, rating, suggested_ev)
    };
    file.write(xml.as_bytes())?;
    Ok(true)
}

/// 仅替换 firstcut 拥有的字段，其他软件增加的元素、属性、处理指令均原样保留。
/// 同时兼容元素写法与 Adobe XMP SDK 规范化后的 RDF 属性 / 自闭合标签写法。
fn merge_xmp(content: &str, e: &PhotoEntry, s: &PixelScores, total: f64,
    rating: u8, suggested_ev: Option<f64>) -> anyhow::Result<String> {
    use anyhow::{anyhow, Context};
    use roxmltree::{Document, Node, NodeType};
    let doc = Document::parse(content).context("现有 XMP 无法解析，已保护原文件")?;
    let desc = doc.descendants().find(|n| n.has_tag_name((NS_RDF, "Description"))
        && n.lookup_prefix(NS_FIRSTCUT).is_some())
        .ok_or_else(|| anyhow!("未找到 firstcut RDF Description，已保护原文件"))?;
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();

    let edit_child_text = |child: Node<'_, '_>, name: &str, value: String,
                           edits: &mut Vec<(std::ops::Range<usize>, String)>| -> anyhow::Result<()> {
        if child.children().any(|n| n.is_element()) {
            return Err(anyhow!("XMP {name} 含嵌套字段，已保护原文件"));
        }
        if let Some(text) = child.children().find(|n| n.node_type() == NodeType::Text) {
            edits.push((text.range(), value));
        } else if child.children().next().is_none() {
            let raw = &content[child.range()];
            let close = raw.rfind("</")
                .ok_or_else(|| anyhow!("XMP {name} 是空标签，已保护原文件"))?;
            let at = child.range().start + close;
            edits.push((at..at, value));
        } else {
            return Err(anyhow!("XMP {name} 不是文本字段，已保护原文件"));
        }
        Ok(())
    };

    // Lightroom / ACR 可能把标量字段写成 RDF 属性或子元素，两种形式都支持。
    let mut set_field = |namespace: &str, name: &str, value: String| -> anyhow::Result<()> {
        let attr = desc.attributes().find(|a| a.namespace() == Some(namespace) && a.name() == name);
        let child = desc.children().find(|n| n.has_tag_name((namespace, name)));
        if attr.is_none() && child.is_none() {
            return Err(anyhow!("XMP 缺少 {name}，已保护原文件"));
        }
        if let Some(attr) = attr {
            edits.push((attr.range_value(), value.clone()));
        }
        if let Some(child) = child {
            edit_child_text(child, name, value, &mut edits)?;
        }
        Ok(())
    };

    set_field(NS_XMP, "Rating", rating.to_string())?;
    for (name, value) in [
        ("sharpness", format!("{:.1}", s.sharpness)),
        ("exposure", format!("{:.1}", s.exposure)),
        ("noise", format!("{:.1}", s.noise)),
        ("composition", format!("{:.1}", s.composition)),
        ("aesthetic", format!("{:.1}", s.aesthetic)),
        ("faces", e.faces.parse::<usize>().unwrap_or(0).to_string()),
        ("total", format!("{total:.1}")),
        ("burstGroup", e.burst_group.clone()),
        ("burstRank", e.burst_rank.clone()),
        ("burstKeep", e.burst_keep.clone()),
    ] {
        set_field(NS_FIRSTCUT, name, value)?;
    }
    drop(set_field);

    let ev_attr = desc.attributes().find(|a| a.namespace() == Some(NS_FIRSTCUT) && a.name() == "suggestedEV");
    let ev_child = desc.children().find(|n| n.has_tag_name((NS_FIRSTCUT, "suggestedEV")));
    let crs_exp_attr = desc.attributes().find(|a| a.namespace() == Some(NS_CRS) && a.name() == "Exposure2012");
    let crs_exp_child = desc.children().find(|n| n.has_tag_name((NS_CRS, "Exposure2012")));
    // 兼容旧版 firstcut 写出的误拼字段 crs:Exposure2，合并时顺手清理升级。
    let legacy_crs_exp2_attr = desc.attributes().find(|a| a.namespace() == Some(NS_CRS) && a.name() == "Exposure2");
    let crs_has_settings_attr = desc.attributes().find(|a| a.namespace() == Some(NS_CRS) && a.name() == "HasSettings");
    let crs_has_settings_child = desc.children().find(|n| n.has_tag_name((NS_CRS, "HasSettings")));
    let crs_pv_attr = desc.attributes().find(|a| a.namespace() == Some(NS_CRS) && a.name() == "ProcessVersion");
    let crs_pv_child = desc.children().find(|n| n.has_tag_name((NS_CRS, "ProcessVersion")));

    let raw_desc = &content[desc.range()];
    let is_self_closing = raw_desc.trim_end().ends_with("/>");
    let mut extra_attrs = String::new();

    match suggested_ev {
        Some(ev) => {
            let formatted_ev = format!("{ev:+.2}");
            if let Some(attr) = ev_attr {
                edits.push((attr.range_value(), formatted_ev.clone()));
            }
            if let Some(child) = ev_child {
                edit_child_text(child, "suggestedEV", formatted_ev.clone(), &mut edits)?;
            }
            if ev_attr.is_none() && ev_child.is_none() {
                let prefix = desc.lookup_prefix(NS_FIRSTCUT).unwrap();
                if is_self_closing {
                    extra_attrs.push_str(&format!(" {prefix}:suggestedEV=\"{formatted_ev}\""));
                } else {
                    let close = raw_desc.rfind("</").ok_or_else(|| anyhow!("XMP Description 无结束标签"))?;
                    let at = desc.range().start + close;
                    edits.push((at..at, format!("<{prefix}:suggestedEV>{formatted_ev}</{prefix}:suggestedEV>")));
                }
            }
            if let Some(attr) = legacy_crs_exp2_attr {
                edits.push((attr.range(), String::new()));
            }
            let crs_prefix = if let Some(prefix) = desc.lookup_prefix(NS_CRS) {
                prefix.to_string()
            } else {
                if desc.lookup_namespace_uri(Some("crs"))
                    .is_some_and(|uri| uri != NS_CRS) {
                    return Err(anyhow!("现有 crs 前缀指向其他命名空间，已保护原文件"));
                }
                extra_attrs.push_str(&format!(" xmlns:crs=\"{NS_CRS}\""));
                "crs".to_string()
            };
            if let Some(attr) = crs_exp_attr {
                edits.push((attr.range_value(), formatted_ev.clone()));
            }
            if let Some(child) = crs_exp_child {
                edit_child_text(child, "Exposure2012", formatted_ev.clone(), &mut edits)?;
            }
            if crs_exp_attr.is_none() && crs_exp_child.is_none() {
                extra_attrs.push_str(&format!(" {crs_prefix}:Exposure2012=\"{formatted_ev}\""));
            }
            if let Some(attr) = crs_has_settings_attr {
                edits.push((attr.range_value(), "True".into()));
            }
            if let Some(child) = crs_has_settings_child {
                edit_child_text(child, "HasSettings", "True".into(), &mut edits)?;
            }
            if crs_has_settings_attr.is_none() && crs_has_settings_child.is_none() {
                extra_attrs.push_str(&format!(" {crs_prefix}:HasSettings=\"True\""));
            }
            if crs_pv_attr.is_none() && crs_pv_child.is_none() {
                extra_attrs.push_str(&format!(" {crs_prefix}:ProcessVersion=\"11.0\""));
            }
        }
        None => {
            if let Some(attr) = ev_attr { edits.push((attr.range(), String::new())); }
            if let Some(child) = ev_child { edits.push((child.range(), String::new())); }
            if let Some(attr) = crs_exp_attr { edits.push((attr.range(), String::new())); }
            if let Some(child) = crs_exp_child { edits.push((child.range(), String::new())); }
            if let Some(attr) = legacy_crs_exp2_attr { edits.push((attr.range(), String::new())); }
            let has_other_crs = desc.attributes().any(|a| {
                a.namespace() == Some(NS_CRS)
                    && !matches!(a.name(), "Exposure2012" | "Exposure2" | "HasSettings" | "ProcessVersion")
            }) || desc.children().any(|n| {
                n.tag_name().namespace() == Some(NS_CRS)
                    && !matches!(n.tag_name().name(), "Exposure2012" | "Exposure2" | "HasSettings" | "ProcessVersion")
            });
            if !has_other_crs {
                if let Some(attr) = crs_has_settings_attr { edits.push((attr.range(), String::new())); }
                if let Some(child) = crs_has_settings_child { edits.push((child.range(), String::new())); }
                if let Some(attr) = crs_pv_attr { edits.push((attr.range(), String::new())); }
                if let Some(child) = crs_pv_child { edits.push((child.range(), String::new())); }
            }
        }
    }

    if !extra_attrs.is_empty() {
        let name_end = raw_desc.find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .ok_or_else(|| anyhow!("XMP Description 起始标签无效"))?;
        let at = desc.range().start + name_end;
        edits.push((at..at, extra_attrs));
    }

    let mut output = content.to_string();
    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start).then_with(|| b.0.end.cmp(&a.0.end)));
    for (range, replacement) in edits {
        output.replace_range(range, &replacement);
    }
    Document::parse(&output).context("合并后的 XMP 无效，已保护原文件")?;
    Ok(output)
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
            rating_source: "algorithm".into(),
            faces: "1".into(),
            analysis_ok: "true".into(),
            analysis_mode: "ai".into(),
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

    /// 建议 EV 存在：写 crs:Exposure2012 + crs:HasSettings + crs:ProcessVersion + firstcut:suggestedEV
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
        assert!(xml.contains(r#"crs:Exposure2012="-0.33""#));
        assert!(xml.contains(r#"crs:HasSettings="True""#));
        assert!(xml.contains(r#"crs:ProcessVersion="11.0""#));
        assert!(xml.contains("xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\""));
        // 建议行位于 total 与 burstGroup 之间
        let i = xml.find("<firstcut:total>").unwrap();
        let j = xml.find("<firstcut:suggestedEV>").unwrap();
        let k = xml.find("<firstcut:burstGroup>").unwrap();
        assert!(i < j && j < k, "suggestedEV 应在 total 之后、burstGroup 之前");
    }

    /// 无建议：侧车不含 crs 命名空间、suggestedEV、Exposure2012
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
        assert!(!xml_none.contains("Exposure2012"), "无建议不应出现 Exposure2012");
        assert!(!xml_none.contains("crs"), "无建议不应出现 crs 命名空间");
        let xml_some = render_xmp(&e, &s, 66.0, 4, Some(1.0));
        assert!(xml_some.contains(r#"<firstcut:suggestedEV>+1.00</firstcut:suggestedEV>"#));
        assert!(xml_some.contains(r#"crs:Exposure2012="+1.00""#));
    }

    #[test]
    fn merge_preserves_unrelated_xmp_and_adds_exposure() {
        let e = entry();
        let s = PixelScores { sharpness: 75.0, exposure: 80.0, noise: 60.0,
            composition: 60.0, aesthetic: 45.0 };
        let original = render_xmp(&e, &s, 66.0, 4, None)
            .replace("xmlns:firstcut=\"http://firstcut.local/ns/\"",
                "xmlns:firstcut=\"http://firstcut.local/ns/\" xmlns:other=\"urn:other\" other:flag=\"keep\"")
            .replace("</rdf:Description>", "<other:note>untouched</other:note></rdf:Description>");
        let updated = merge_xmp(&original, &e, &s, 66.0, 2, Some(0.67)).unwrap();
        assert!(updated.contains("other:flag=\"keep\""));
        assert!(updated.contains("<other:note>untouched</other:note>"));
        assert!(updated.contains("<xmp:Rating>2</xmp:Rating>"));
        assert!(updated.contains("crs:Exposure2012=\"+0.67\""));
        assert!(updated.contains("crs:HasSettings=\"True\""));
        assert!(updated.contains("crs:ProcessVersion=\"11.0\""));
        assert!(updated.contains("<firstcut:suggestedEV>+0.67</firstcut:suggestedEV>"));
        let cleared = merge_xmp(&updated, &e, &s, 66.0, 3, None).unwrap();
        assert!(!cleared.contains("crs:Exposure2012="));
        assert!(!cleared.contains("crs:HasSettings="));
        assert!(!cleared.contains("<firstcut:suggestedEV>"));
        assert!(cleared.contains("other:flag=\"keep\""));
        let duplicate_rating = original.replace(
            "xmlns:firstcut=\"http://firstcut.local/ns/\"",
            "xmp:Rating=\"4\" xmlns:firstcut=\"http://firstcut.local/ns/\"",
        );
        let normalized = merge_xmp(&duplicate_rating, &e, &s, 66.0, 2, None).unwrap();
        assert!(normalized.contains("xmp:Rating=\"2\""));
        assert!(normalized.contains("<xmp:Rating>2</xmp:Rating>"));
    }

    /// Adobe XMP SDK 规范化为属性 + 自闭合标签 + 保留外部 CRS 编辑与旧版 Exposure2 迁移
    #[test]
    fn merge_handles_adobe_attribute_form_and_preserves_external_crs() {
        let e = entry();
        let s = PixelScores { sharpness: 78.0, exposure: 65.0, noise: 70.0,
            composition: 62.0, aesthetic: 55.0 };
        let adobe_xmp = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description rdf:about=\"\" ",
            "xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" ",
            "xmlns:firstcut=\"http://firstcut.local/ns/\" ",
            "xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" ",
            "xmp:Rating=\"4\" ",
            "firstcut:sharpness=\"75.0\" firstcut:exposure=\"80.0\" ",
            "firstcut:noise=\"60.0\" firstcut:composition=\"60.0\" ",
            "firstcut:aesthetic=\"45.0\" firstcut:faces=\"1\" ",
            "firstcut:total=\"66.0\" firstcut:burstGroup=\"3\" ",
            "firstcut:burstRank=\"1\" firstcut:burstKeep=\"true\" ",
            "crs:HasSettings=\"True\" crs:ProcessVersion=\"15.4\" ",
            "crs:Contrast2012=\"+15\" crs:Exposure2=\"0.33\"/>",
            "</rdf:RDF></x:xmpmeta>",
        );
        let upgraded = merge_xmp(adobe_xmp, &e, &s, 68.5, 5, Some(1.0)).unwrap();
        assert!(upgraded.contains("xmp:Rating=\"5\""));
        assert!(upgraded.contains("firstcut:sharpness=\"78.0\""));
        assert!(upgraded.contains("firstcut:suggestedEV=\"+1.00\""));
        assert!(upgraded.contains("crs:Exposure2012=\"+1.00\""));
        assert!(!upgraded.contains("crs:Exposure2=\""));
        assert!(upgraded.contains("crs:ProcessVersion=\"15.4\""), "不应降级 Lightroom 已有 ProcessVersion");
        assert!(upgraded.contains("crs:Contrast2012=\"+15\""), "应保留用户其他 CRS 编辑");

        let cleared = merge_xmp(&upgraded, &e, &s, 68.5, 3, None).unwrap();
        assert!(!cleared.contains("crs:Exposure2012="));
        assert!(!cleared.contains("firstcut:suggestedEV="));
        assert!(cleared.contains("crs:HasSettings=\"True\""), "存在其他 CRS 编辑时应保留 HasSettings");
        assert!(cleared.contains("crs:ProcessVersion=\"15.4\""));
        assert!(cleared.contains("crs:Contrast2012=\"+15\""));
    }
}
