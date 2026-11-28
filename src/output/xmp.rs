//! XMP 侧车写出（M4/M7）
//!
//! 为每张照片写侧车 `<stem>.xmp`（Lightroom / Camera Raw / darktable 均可读）：
//! - `xmp:Rating`（1-5 星）
//! - `firstcut:` 自定义命名空间存 5 维子分 + 人脸数 + 连拍信息
//! - 建议曝光 EV（仅曝光出容差带时）：常规评分仅写信息字段 `firstcut:suggestedEV`；
//!   显式启用 `--xmp-exposure`（或 `exposure_write_crs = true`）时，才在尚无已有
//!   `crs:Exposure2012` 的侧车中写入 Adobe 开发字段（`crs:Exposure2012` + `crs:HasSettings="True"`
//!   + `crs:ProcessVersion="11.0"`），绝不覆盖或删除已有的人工曝光调整。
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

/// XMP 侧车写入策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmpWriteMode {
    /// 常规评分：同步星级、五维子分、连拍与 `firstcut:suggestedEV`，不触碰 Adobe `crs:*` 开发参数。
    MetadataOnly,
    /// 显式应用建议曝光：除常规元数据外，若侧车尚无 `crs:Exposure2012` / `crs:Exposure`，
    /// 则写入 `crs:Exposure2012` + `crs:HasSettings="True"` + `crs:ProcessVersion="11.0"`；
    /// 绝不覆盖或删除已有的人工曝光参数。
    ApplySuggestedExposure,
    /// 人工改星：已有 firstcut 侧车时**仅更新 `xmp:Rating`**，不改动 `firstcut:suggestedEV`
    /// 或任何 `crs:*` 字段；侧车不存在时按 `MetadataOnly` 新建。
    RatingOnly,
}

/// 生成全新 XMP 侧车内容（默认不写 Adobe `crs:*` 开发字段，见 [`render_xmp_with_crs`]）。
pub fn render_xmp(
    e: &PhotoEntry,
    s: &PixelScores,
    total: f64,
    rating: u8,
    suggested_ev: Option<f64>,
) -> String {
    render_xmp_with_crs(e, s, total, rating, suggested_ev, false)
}

/// 生成全新 XMP 侧车内容；`write_crs = true` 时一并写入 Adobe `crs:Exposure2012` 开发字段。
pub fn render_xmp_with_crs(
    e: &PhotoEntry,
    s: &PixelScores,
    total: f64,
    rating: u8,
    suggested_ev: Option<f64>,
    write_crs: bool,
) -> String {
    let faces = e.faces.parse::<usize>().unwrap_or(0);
    // 建议曝光 EV：始终写入信息字段 `firstcut:suggestedEV`；
    // 仅当用户显式选择 `write_crs` 时才附加 Adobe 开发字段，避免无意改变 Lightroom 初始曝光。
    let (crs_ns, crs_attr, ev_line) = match suggested_ev {
        Some(ev) => (
            if write_crs {
                String::from(" xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"")
            } else {
                String::new()
            },
            if write_crs {
                format!(
                    " crs:HasSettings=\"True\" crs:ProcessVersion=\"11.0\" crs:Exposure2012=\"{ev:+.2}\""
                )
            } else {
                String::new()
            },
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

/// 生成仅包含人工星级的最小 XMP 侧车（用于尚未评分但已手动打星的照片；不含 `xmlns:firstcut`）。
pub fn render_rating_only_xmp(rating: u8) -> String {
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         \x20<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         \x20\x20<rdf:Description rdf:about=\"\"\n\
         \x20\x20\x20 xmlns:xmp=\"{NS_XMP}\">\n\
         \x20\x20\x20\x20<xmp:Rating>{rating}</xmp:Rating>\n\
         \x20\x20</rdf:Description>\n\
         \x20</rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>"
    )
}

/// 写出侧车（默认 `MetadataOnly`）；他人侧车（无 firstcut 命名空间）不覆盖。
/// 返回 Ok(true) 表示已写入，Ok(false) 表示跳过（他人侧车/无分数）。
pub fn write_sidecar(
    root: &std::path::Path,
    e: &PhotoEntry,
    s: &PixelScores,
    total: f64,
    rating: u8,
    suggested_ev: Option<f64>,
) -> anyhow::Result<bool> {
    write_sidecar_with_mode(
        root,
        e,
        s,
        total,
        rating,
        suggested_ev,
        XmpWriteMode::MetadataOnly,
    )
}

/// 按指定模式写出或合并 XMP 侧车；他人侧车（无 firstcut 命名空间）永不覆盖。
pub fn write_sidecar_with_mode(
    root: &std::path::Path,
    e: &PhotoEntry,
    s: &PixelScores,
    total: f64,
    rating: u8,
    suggested_ev: Option<f64>,
    mode: XmpWriteMode,
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
        merge_xmp(&content, e, s, total, rating, suggested_ev, mode)?
    } else {
        let write_crs = mode == XmpWriteMode::ApplySuggestedExposure;
        render_xmp_with_crs(e, s, total, rating, suggested_ev, write_crs)
    };
    file.write(xml.as_bytes())?;
    Ok(true)
}

/// 导出专用：在内存中把最新评分与星级合并进已有 XMP 文本（绝不修改源文件）。
/// - 若已有 `firstcut:` 命名空间，沿用 `merge_xmp`（遵守方案 A，绝不覆盖已有 `crs:Exposure2012`）；
/// - 若为外部 Lightroom/ACR 侧车，仅在内存中更新（或补充）`xmp:Rating`，其余字段与 `crs:*` 保持原样。
pub fn merge_xmp_for_export(
    existing_content: &str,
    e: &PhotoEntry,
    s: &PixelScores,
    total: f64,
    rating: u8,
    suggested_ev: Option<f64>,
    write_crs: bool,
) -> anyhow::Result<String> {
    if existing_content.contains(NS_FIRSTCUT) {
        let mode = if write_crs {
            XmpWriteMode::ApplySuggestedExposure
        } else {
            XmpWriteMode::MetadataOnly
        };
        return merge_xmp(existing_content, e, s, total, rating, suggested_ev, mode);
    }
    update_external_xmp_rating(existing_content, rating)
}

/// 导出专用：对尚未评分但已有人工星级的照片，仅在内存中更新或补充 `xmp:Rating`
/// （保留原有 `crs:Exposure2012` 与任何现有 `firstcut:*` 字段）。
pub fn merge_xmp_rating_only_for_export(
    existing_content: &str,
    rating: u8,
) -> anyhow::Result<String> {
    update_external_xmp_rating(existing_content, rating)
}

fn update_external_xmp_rating(existing_content: &str, rating: u8) -> anyhow::Result<String> {
    use anyhow::{anyhow, Context};
    use roxmltree::{Document, NodeType};
    let doc = Document::parse(existing_content).context("现有 XMP 无法解析")?;
    let mut output = existing_content.to_string();

    if let Some(desc) = doc
        .descendants()
        .find(|n| n.has_tag_name((NS_RDF, "Description"))
            && (n.attributes().any(|a| a.namespace() == Some(NS_XMP) && a.name() == "Rating")
                || n.children().any(|c| c.has_tag_name((NS_XMP, "Rating")))))
    {
        let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        if let Some(attr) = desc
            .attributes()
            .find(|a| a.namespace() == Some(NS_XMP) && a.name() == "Rating")
        {
            edits.push((attr.range_value(), rating.to_string()));
        }
        if let Some(child) = desc.children().find(|c| c.has_tag_name((NS_XMP, "Rating"))) {
            if let Some(text) = child.children().find(|n| n.node_type() == NodeType::Text) {
                edits.push((text.range(), rating.to_string()));
            } else if child.children().next().is_none() {
                let raw = &existing_content[child.range()];
                let close = raw
                    .rfind("</")
                    .ok_or_else(|| anyhow!("XMP Rating 是空标签"))?;
                let at = child.range().start + close;
                edits.push((at..at, rating.to_string()));
            } else {
                return Err(anyhow!("XMP Rating 不是纯文本字段"));
            }
        }
        edits.sort_by(|a, b| b.0.start.cmp(&a.0.start).then_with(|| b.0.end.cmp(&a.0.end)));
        for (range, replacement) in edits {
            output.replace_range(range, &replacement);
        }
        Document::parse(&output).context("合并后的 XMP 无效")?;
        return Ok(output);
    }

    let primary = doc
        .descendants()
        .find(|n| n.has_tag_name((NS_RDF, "Description")))
        .ok_or_else(|| anyhow!("未找到 rdf:Description"))?;
    let raw = &existing_content[primary.range()];
    let close = raw
        .rfind("</rdf:Description>")
        .ok_or_else(|| anyhow!("无法定位 rdf:Description 结束标签"))?;
    let has_xmp_ns = primary.lookup_prefix(NS_XMP).is_some();
    let ns_attr = if has_xmp_ns {
        ""
    } else {
        " xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\""
    };
    let at = primary.range().start + close;
    output.insert_str(
        at,
        &format!("    <xmp:Rating{ns_attr}>{rating}</xmp:Rating>\n  "),
    );
    Document::parse(&output).context("合并后的 XMP 无效")?;
    Ok(output)
}

/// 仅替换 firstcut 拥有的字段，其他软件增加的元素、属性、处理指令均原样保留。
/// 同时兼容元素写法与 Adobe XMP SDK 规范化后的 RDF 属性 / 自闭合标签写法。
/// 绝不覆盖或删除已有的 Adobe `crs:Exposure2012` / `crs:Exposure` 人工曝光字段。
fn merge_xmp(
    content: &str,
    e: &PhotoEntry,
    s: &PixelScores,
    total: f64,
    rating: u8,
    suggested_ev: Option<f64>,
    mode: XmpWriteMode,
) -> anyhow::Result<String> {
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
    if mode == XmpWriteMode::RatingOnly {
        drop(set_field);
        let mut output = content.to_string();
        edits.sort_by(|a, b| b.0.start.cmp(&a.0.start).then_with(|| b.0.end.cmp(&a.0.end)));
        for (range, replacement) in edits {
            output.replace_range(range, &replacement);
        }
        Document::parse(&output).context("合并后的 XMP 无效，已保护原文件")?;
        return Ok(output);
    }

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
            // 仅在显式请求且侧车尚无已有 Adobe 曝光参数时写入 crs:Exposure2012；绝不覆盖人工曝光。
            if mode == XmpWriteMode::ApplySuggestedExposure {
                // Adobe XMP 规范 §7.4 允许同一张照片的属性分布在 <rdf:RDF> 下的多个顶层
                // <rdf:Description> 节点中；检查所有顶层描述节点，避免在已有曝光时重复写入，
                // 同时不误判局部蒙版/预设等嵌套 <rdf:Description>。
                let top_descriptions: Vec<Node<'_, '_>> = desc
                    .parent()
                    .filter(|p| p.has_tag_name((NS_RDF, "RDF")))
                    .map(|rdf| {
                        rdf.children()
                            .filter(|n| n.has_tag_name((NS_RDF, "Description")))
                            .collect()
                    })
                    .unwrap_or_else(|| vec![desc]);
                let has_existing_crs_exposure = top_descriptions.iter().any(|d| {
                    d.attributes().any(|a| {
                        a.namespace() == Some(NS_CRS)
                            && matches!(a.name(), "Exposure2012" | "Exposure")
                    }) || d.children().any(|n| {
                        n.tag_name().namespace() == Some(NS_CRS)
                            && matches!(n.tag_name().name(), "Exposure2012" | "Exposure")
                    })
                });
                for d in &top_descriptions {
                    if let Some(legacy_exp2) = d
                        .attributes()
                        .find(|a| a.namespace() == Some(NS_CRS) && a.name() == "Exposure2")
                    {
                        edits.push((legacy_exp2.range(), String::new()));
                    }
                }
                if !has_existing_crs_exposure {
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
                    extra_attrs.push_str(&format!(" {crs_prefix}:Exposure2012=\"{formatted_ev}\""));
                    let mut updated_has_settings = false;
                    for d in &top_descriptions {
                        if let Some(attr) = d
                            .attributes()
                            .find(|a| a.namespace() == Some(NS_CRS) && a.name() == "HasSettings")
                        {
                            edits.push((attr.range_value(), "True".into()));
                            updated_has_settings = true;
                        }
                        if let Some(child) =
                            d.children().find(|n| n.has_tag_name((NS_CRS, "HasSettings")))
                        {
                            edit_child_text(child, "HasSettings", "True".into(), &mut edits)?;
                            updated_has_settings = true;
                        }
                    }
                    if !updated_has_settings {
                        extra_attrs.push_str(&format!(" {crs_prefix}:HasSettings=\"True\""));
                    }
                    let has_pv = top_descriptions.iter().any(|d| {
                        d.attributes().any(|a| a.namespace() == Some(NS_CRS) && a.name() == "ProcessVersion")
                            || d.children().any(|n| n.has_tag_name((NS_CRS, "ProcessVersion")))
                    });
                    if !has_pv {
                        extra_attrs.push_str(&format!(" {crs_prefix}:ProcessVersion=\"11.0\""));
                    }
                }
            }
        }
        None => {
            if let Some(attr) = ev_attr { edits.push((attr.range(), String::new())); }
            if let Some(child) = ev_child { edits.push((child.range(), String::new())); }
            // 绝不删除已有的 crs:Exposure2012 / crs:HasSettings / crs:ProcessVersion，
            // 避免误删摄影师在 Lightroom/ACR 中的手动曝光设置。
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
            curation_flag: String::new(),
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

    /// 建议 EV 存在：默认只写信息字段 firstcut:suggestedEV，显式开启 write_crs 才写 Adobe 开发参数
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
        let meta_only = render_xmp(&e, &s, 55.0, 2, Some(-0.33));
        assert!(meta_only.contains(r#"<firstcut:suggestedEV>-0.33</firstcut:suggestedEV>"#));
        assert!(!meta_only.contains("crs:Exposure2012"), "默认模式不应主动写入 Adobe 开发参数");
        assert!(!meta_only.contains("xmlns:crs"));
        // 建议行位于 total 与 burstGroup 之间
        let i = meta_only.find("<firstcut:total>").unwrap();
        let j = meta_only.find("<firstcut:suggestedEV>").unwrap();
        let k = meta_only.find("<firstcut:burstGroup>").unwrap();
        assert!(i < j && j < k, "suggestedEV 应在 total 之后、burstGroup 之前");

        let with_crs = render_xmp_with_crs(&e, &s, 55.0, 2, Some(-0.33), true);
        assert!(with_crs.contains(r#"<firstcut:suggestedEV>-0.33</firstcut:suggestedEV>"#));
        assert!(with_crs.contains(r#"crs:Exposure2012="-0.33""#));
        assert!(with_crs.contains(r#"crs:HasSettings="True""#));
        assert!(with_crs.contains(r#"crs:ProcessVersion="11.0""#));
        assert!(with_crs.contains("xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\""));
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
        let xml_none = render_xmp_with_crs(&e, &s, 66.0, 4, None, true);
        assert!(!xml_none.contains("suggestedEV"), "无建议不应出现 suggestedEV");
        assert!(!xml_none.contains("Exposure2012"), "无建议不应出现 Exposure2012");
        assert!(!xml_none.contains("crs"), "无建议不应出现 crs 命名空间");
        let xml_some = render_xmp_with_crs(&e, &s, 66.0, 4, Some(1.0), true);
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

        // MetadataOnly：只写 firstcut:suggestedEV，不触碰 crs:*
        let meta_updated = merge_xmp(&original, &e, &s, 66.0, 2, Some(0.67), XmpWriteMode::MetadataOnly).unwrap();
        assert!(meta_updated.contains("other:flag=\"keep\""));
        assert!(meta_updated.contains("<other:note>untouched</other:note>"));
        assert!(meta_updated.contains("<xmp:Rating>2</xmp:Rating>"));
        assert!(meta_updated.contains("<firstcut:suggestedEV>+0.67</firstcut:suggestedEV>"));
        assert!(!meta_updated.contains("crs:Exposure2012"));

        // ApplySuggestedExposure：在尚无 crs:Exposure2012 时写入 Adobe 开发参数
        let crs_updated = merge_xmp(&original, &e, &s, 66.0, 2, Some(0.67), XmpWriteMode::ApplySuggestedExposure).unwrap();
        assert!(crs_updated.contains("crs:Exposure2012=\"+0.67\""));
        assert!(crs_updated.contains("crs:HasSettings=\"True\""));
        assert!(crs_updated.contains("crs:ProcessVersion=\"11.0\""));
        assert!(crs_updated.contains("<firstcut:suggestedEV>+0.67</firstcut:suggestedEV>"));

        // 无建议时只清理 firstcut:suggestedEV，绝不删除已有 crs:Exposure2012
        let cleared = merge_xmp(&crs_updated, &e, &s, 66.0, 3, None, XmpWriteMode::MetadataOnly).unwrap();
        assert!(!cleared.contains("<firstcut:suggestedEV>"));
        assert!(cleared.contains("crs:Exposure2012=\"+0.67\""), "绝不应删除已有 crs:Exposure2012");
        assert!(cleared.contains("other:flag=\"keep\""));

        let duplicate_rating = original.replace(
            "xmlns:firstcut=\"http://firstcut.local/ns/\"",
            "xmp:Rating=\"4\" xmlns:firstcut=\"http://firstcut.local/ns/\"",
        );
        let normalized = merge_xmp(&duplicate_rating, &e, &s, 66.0, 2, None, XmpWriteMode::RatingOnly).unwrap();
        assert!(normalized.contains("xmp:Rating=\"2\""));
        assert!(normalized.contains("<xmp:Rating>2</xmp:Rating>"));
    }

    /// 改星与常规/显式合并都绝不覆盖或删除摄影师在 Lightroom 中手动设置的 crs:Exposure2012
    #[test]
    fn existing_lightroom_exposure_is_never_overwritten_or_deleted() {
        let e = entry();
        let s = PixelScores { sharpness: 78.0, exposure: 65.0, noise: 70.0,
            composition: 62.0, aesthetic: 55.0 };
        let edited_in_lr = concat!(
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
            "firstcut:suggestedEV=\"+0.33\" ",
            "crs:HasSettings=\"True\" crs:ProcessVersion=\"15.4\" ",
            "crs:Exposure2012=\"+1.75\" crs:Contrast2012=\"+15\"/>",
            "</rdf:RDF></x:xmpmeta>",
        );

        // 1. 仅改星（带建议 EV 或无建议 EV）：只改 xmp:Rating，保留 +1.75 与所有其他字段
        for ev in [Some(0.33), None] {
            let rated = merge_xmp(edited_in_lr, &e, &s, 68.5, 5, ev, XmpWriteMode::RatingOnly).unwrap();
            assert!(rated.contains("xmp:Rating=\"5\""));
            assert!(rated.contains("crs:Exposure2012=\"+1.75\""), "改星绝不能改写或删除已有人工曝光");
            assert!(rated.contains("firstcut:suggestedEV=\"+0.33\""), "改星不应改写 suggestedEV");
            assert!(rated.contains("firstcut:sharpness=\"75.0\""), "改星不应改写子分");
            assert!(rated.contains("crs:ProcessVersion=\"15.4\""));
            assert!(rated.contains("crs:Contrast2012=\"+15\""));
        }

        // 2. 常规评分 MetadataOnly 与显式 ApplySuggestedExposure（带建议或无建议）：均保留 +1.75
        for mode in [XmpWriteMode::MetadataOnly, XmpWriteMode::ApplySuggestedExposure] {
            let with_ev = merge_xmp(edited_in_lr, &e, &s, 68.5, 5, Some(0.33), mode).unwrap();
            assert!(with_ev.contains("crs:Exposure2012=\"+1.75\""), "{mode:?} 不得覆盖已有人工曝光");
            assert!(with_ev.contains("firstcut:suggestedEV=\"+0.33\""));
            let without_ev = merge_xmp(edited_in_lr, &e, &s, 68.5, 5, None, mode).unwrap();
            assert!(without_ev.contains("crs:Exposure2012=\"+1.75\""), "{mode:?} 无建议时不得删除已有人工曝光");
            assert!(!without_ev.contains("firstcut:suggestedEV="));
        }

        // 3. 尚无 crs:Exposure2012（仅旧版误拼 crs:Exposure2）时，显式模式才写入并清理旧误拼字段
        let legacy_only = edited_in_lr.replace("crs:Exposure2012=\"+1.75\"", "crs:Exposure2=\"0.33\"");
        let applied = merge_xmp(&legacy_only, &e, &s, 68.5, 5, Some(1.0), XmpWriteMode::ApplySuggestedExposure).unwrap();
        assert!(applied.contains("crs:Exposure2012=\"+1.00\""));
        assert!(!applied.contains("crs:Exposure2=\""));
        assert!(applied.contains("crs:ProcessVersion=\"15.4\""), "不应降级 Lightroom 已有 ProcessVersion");
    }

    /// XMP §7.4 多顶层 rdf:Description 场景：已有曝光位于第二个描述节点时，
    /// 显式 ApplySuggestedExposure 不得再往第一个节点重复写入 crs:Exposure2012。
    #[test]
    fn split_rdf_descriptions_do_not_duplicate_crs_exposure() {
        let e = entry();
        let s = PixelScores { sharpness: 78.0, exposure: 65.0, noise: 70.0,
            composition: 62.0, aesthetic: 55.0 };
        let split_with_exposure = concat!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">",
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">",
            "<rdf:Description rdf:about=\"\" ",
            "xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" ",
            "xmlns:firstcut=\"http://firstcut.local/ns/\">",
            "<xmp:Rating>3</xmp:Rating>",
            "<firstcut:sharpness>75.0</firstcut:sharpness>",
            "<firstcut:exposure>80.0</firstcut:exposure>",
            "<firstcut:noise>60.0</firstcut:noise>",
            "<firstcut:composition>60.0</firstcut:composition>",
            "<firstcut:aesthetic>45.0</firstcut:aesthetic>",
            "<firstcut:faces>1</firstcut:faces>",
            "<firstcut:total>66.0</firstcut:total>",
            "<firstcut:burstGroup>3</firstcut:burstGroup>",
            "<firstcut:burstRank>1</firstcut:burstRank>",
            "<firstcut:burstKeep>true</firstcut:burstKeep>",
            "</rdf:Description>",
            "<rdf:Description rdf:about=\"\" ",
            "xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" ",
            "crs:HasSettings=\"True\" crs:ProcessVersion=\"15.4\" ",
            "crs:Exposure2012=\"+1.75\"/>",
            "</rdf:RDF></x:xmpmeta>",
        );

        let merged = merge_xmp(
            split_with_exposure,
            &e,
            &s,
            68.5,
            4,
            Some(0.33),
            XmpWriteMode::ApplySuggestedExposure,
        )
        .unwrap();
        assert_eq!(
            merged.matches("Exposure2012").count(),
            1,
            "已有顶层 Exposure2012 时绝不能生成第二个曝光值"
        );
        assert!(merged.contains("crs:Exposure2012=\"+1.75\""));
        assert!(merged.contains("<firstcut:suggestedEV>+0.33</firstcut:suggestedEV>"));

        // 第二个顶层节点有 ProcessVersion/HasSettings 但无曝光时，补写曝光也不应重复生成 ProcessVersion/HasSettings
        let split_without_exposure = split_with_exposure.replace(
            "crs:HasSettings=\"True\" crs:ProcessVersion=\"15.4\" crs:Exposure2012=\"+1.75\"",
            "crs:HasSettings=\"False\" crs:ProcessVersion=\"15.4\" crs:Shadows2012=\"+20\"",
        );
        let merged_no_exp = merge_xmp(
            &split_without_exposure,
            &e,
            &s,
            68.5,
            4,
            Some(0.33),
            XmpWriteMode::ApplySuggestedExposure,
        )
        .unwrap();
        assert_eq!(merged_no_exp.matches("Exposure2012").count(), 1);
        assert!(merged_no_exp.contains("crs:Exposure2012=\"+0.33\""));
        assert_eq!(merged_no_exp.matches("HasSettings").count(), 1);
        assert!(merged_no_exp.contains("crs:HasSettings=\"True\""));
        assert_eq!(merged_no_exp.matches("ProcessVersion").count(), 1);
        assert!(merged_no_exp.contains("crs:ProcessVersion=\"15.4\""));
    }

    #[test]
    fn rating_only_xmp_and_export_merge_preserve_crs_exposure() {
        let minimal = render_rating_only_xmp(4);
        assert!(minimal.contains("<xmp:Rating>4</xmp:Rating>"));
        assert!(!minimal.contains("xmlns:firstcut"));
        roxmltree::Document::parse(&minimal).unwrap();

        let lr_xmp = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
            <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
              <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" xmp:Rating=\"2\" crs:Exposure2012=\"+0.75\"/>\n\
             </rdf:RDF>\n\
            </x:xmpmeta>\n\
            <?xpacket end=\"w\"?>";
        let merged = merge_xmp_rating_only_for_export(lr_xmp, 5).unwrap();
        assert!(merged.contains("xmp:Rating=\"5\""));
        assert!(merged.contains("crs:Exposure2012=\"+0.75\""));
    }
}
