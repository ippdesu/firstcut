//! 曝光/直方图分析（M1/M6）
//!
//! 曝光判定由两部分组成：
//! 1. 死白/死黑像素比例（硬性缺陷，直接扣分）
//! 2. "用于判定的亮度"偏离理想区的程度（软性偏好，平顶容差带）
//!
//! "用于判定的亮度"在有主体级人脸时取「全图截尾均值」与「主体脸区域均值」的
//! 加权混合：舞台黑幕布/暗厅会拉低全图均值，而白裙/白毛毯会抬高全图均值，
//! 两者的共同点是「全图均值不代表主体」，混入主体脸亮度可同时修正两类误判。

use crate::decode::{AnalyzedImage, OVEREXPOSED_THRESHOLD, UNDEREXPOSED_THRESHOLD};

/// 曝光统计
pub struct ExposureStats {
    /// 过曝像素比例（0-1）
    pub over_ratio: f64,
    /// 欠曝像素比例（0-1）
    pub under_ratio: f64,
    /// 平均亮度（0-255）
    pub mean: f64,
    /// 排除最暗 25% 像素后的平均亮度：用于曝光判定，
    /// 避免高反差场景（舞台黑幕布/夜景背景）把整体均值拉低而误判欠曝
    pub mean_trunc: f64,
}

pub fn exposure_stats(img: &AnalyzedImage) -> ExposureStats {
    let total = img.width as f64 * img.height as f64;
    if total == 0.0 {
        return ExposureStats { over_ratio: 0.0, under_ratio: 0.0, mean: 0.0, mean_trunc: 0.0 };
    }
    let over: u64 = img.histogram[OVEREXPOSED_THRESHOLD as usize..].iter().sum();
    let under: u64 = img.histogram[..=UNDEREXPOSED_THRESHOLD as usize].iter().sum();
    let mean: f64 = img
        .histogram
        .iter()
        .enumerate()
        .map(|(v, c)| v as f64 * *c as f64)
        .sum::<f64>()
        / total;

    // 排除最暗 25% 像素后的均值（截尾均值）
    let cutoff = (total * 0.25).ceil() as u64;
    let mut acc = 0u64;
    let mut start = 0usize;
    for (v, c) in img.histogram.iter().enumerate() {
        acc += c;
        if acc >= cutoff {
            start = v;
            break;
        }
    }
    let (sum, n): (u64, u64) = img.histogram[start..]
        .iter()
        .enumerate()
        .fold((0, 0), |(s, k), (i, c)| (s + (start + i) as u64 * c, k + c));
    let mean_trunc = if n == 0 { mean } else { sum as f64 / n as f64 };

    ExposureStats {
        over_ratio: over as f64 / total,
        under_ratio: under as f64 / total,
        mean,
        mean_trunc,
    }
}

/// 曝光评分曲线参数（以曝光档位 EV 表达，与相机/场景无关）
///
/// 曲线在 `[-ev_full_lo, +ev_full_hi]` 档内给满分，超出后按档位线性衰减：
/// 暗侧到 `-ev_lo` 档、亮侧到 `+ev_hi` 档时降为 0。
///
/// 两侧独立，是因为"允许暗"和"允许亮"在摄影上不是同一件事：
/// 舞台/夜景要放宽暗侧，但高光依然不能溢出；雪景/白裙要放宽亮侧，
/// 但暗部依然不能死黑。用单一容差会把两侧一起放宽。
///
/// 为什么不用高斯：高斯从中心就开始衰减，对「整片暗背景」或「整片白背景」的
/// 合法场景（舞台黑幕布、白裙、雪景）惩罚过重；而 EV 容差带只惩罚真正越界的曝光。
#[derive(Debug, Clone, Copy)]
pub struct ExposureCurve {
    /// 理想亮度（0-255 码值，默认 128 中灰）
    pub target: f64,
    /// 暗侧满分容差（EV）：0 ~ -此档位内视为正确曝光
    pub ev_full_lo: f64,
    /// 亮侧满分容差（EV）：0 ~ +此档位内视为正确曝光
    pub ev_full_hi: f64,
    /// 暗侧衰减到 0 的档位（相对目标，正数，须大于 ev_full_lo）
    pub ev_lo: f64,
    /// 亮侧衰减到 0 的档位（相对目标，正数，须大于 ev_full_hi）
    pub ev_hi: f64,
    /// 主体脸亮度权重（0 = 只看全图，1 = 只看主体脸）
    pub subject_blend: f64,
    /// 建议 EV 上限（档）：带外照片的建议修正被钳在 ±此值内；0 = 不给建议
    pub suggest_cap: f64,
}

/// sRGB 码值 → 线性光
fn srgb_to_linear(code: f64) -> f64 {
    let s = (code / 255.0).clamp(0.0, 1.0);
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// 码值相对目标亮度的曝光偏移（单位：EV / 档）
pub fn ev_offset(code: f64, target: f64) -> f64 {
    let lt = srgb_to_linear(target).max(1e-6);
    let lx = srgb_to_linear(code).max(1e-6);
    (lx / lt).log2()
}

/// EV 容差带的色调分（0-1）
fn tone_term(code: f64, c: &ExposureCurve) -> f64 {
    let ev = ev_offset(code, c.target);
    if ev >= -c.ev_full_lo && ev <= c.ev_full_hi {
        1.0
    } else if ev < -c.ev_full_lo {
        let span = (c.ev_lo - c.ev_full_lo).max(1e-6);
        1.0 - ((-ev - c.ev_full_lo) / span)
    } else {
        let span = (c.ev_hi - c.ev_full_hi).max(1e-6);
        1.0 - ((ev - c.ev_full_hi) / span)
    }
    .clamp(0.0, 1.0)
}

/// 判定亮度（0-255 码值）：全图截尾均值 + 主体脸的单向拉回混合
///
/// 主体亮度只用于**把被背景误导的判定拉回中灰方向**：
/// - 判定值先夹到 `[min(全图, 目标), max(全图, 目标)]` 之间，
///   所以它永远不会越过目标把判定推到另一侧；
/// - 再按 `subject_blend` 混合。
///
/// 这样设计的原因是两种估计各有偏差：全图截尾均值被背景面积带偏
/// （舞台黑幕布偏低、白裙白背景偏高），主体脸均值被脸框内的头发/阴影带偏。
/// 让主体信息只做"拉回中灰"的单向修正，可以救回被背景误判的照片，
/// 又不会因为一个暗色/亮色误检框而把本来正常的照片打下去。
pub fn exposure_basis(stats: &ExposureStats, subject_mean: Option<f64>, c: &ExposureCurve) -> f64 {
    let blend = c.subject_blend.clamp(0.0, 1.0);
    match subject_mean {
        Some(s) => {
            let lo = stats.mean_trunc.min(c.target);
            let hi = stats.mean_trunc.max(c.target);
            let pull = s.clamp(lo, hi) - stats.mean_trunc;
            stats.mean_trunc + blend * pull
        }
        None => stats.mean_trunc,
    }
}

/// 曝光分数（0-100，100 最佳）
///
/// `subject_mean`：主体级人脸区域的平均亮度（无人脸时 None）。
pub fn exposure_score(
    stats: &ExposureStats,
    subject_mean: Option<f64>,
    c: &ExposureCurve,
) -> f64 {
    let basis = exposure_basis(stats, subject_mean, c);
    let clip_penalty = 4.0 * stats.over_ratio + 4.0 * stats.under_ratio;
    (100.0 * (1.0 - clip_penalty).max(0.0) * tone_term(basis, c)).clamp(0.0, 100.0)
}

/// 建议曝光修正（EV，1/3 档取整；None = 不需要建议）
///
/// 与打分共用同一判定亮度：
/// - 判定值落在满分容差带内（与打分同带）→ 正常照片，不给建议；
/// - 带外 → 建议反向修正（暗 → +EV，亮 → −EV），钳位在 ±`suggest_cap`
///   （极欠/极曝的照片应该在筛选里被淘汰，而不是被建议 +3 档这种鬼数值）；
/// - `suggest_cap = 0` → 一律不给建议。
///
/// 建议值同时写进 XMP（`firstcut:suggestedEV` 信息字段 + `crs:Exposure2`
/// Lightroom 开发字段），实际曝光修正由用户在 Lightroom 里确认/应用，
/// 本程序不修改照片。
pub fn suggested_ev(stats: &ExposureStats, subject_mean: Option<f64>, c: &ExposureCurve) -> Option<f64> {
    let cap = c.suggest_cap;
    if cap <= 0.0 {
        return None;
    }
    let basis = exposure_basis(stats, subject_mean, c);
    let ev = ev_offset(basis, c.target); // 负 = 偏暗
    if ev >= -c.ev_full_lo && ev <= c.ev_full_hi {
        return None; // 容差带内 = 曝光正常，不折腾
    }
    let corr = -ev;
    // 先 1/3 档取整、再钳位：钳位在取整之后才能保证建议值不越过 cap
    let rounded = (corr * 3.0).round() / 3.0;
    let clamped = rounded.clamp(-cap, cap);
    if clamped.abs() < 1e-9 {
        None
    } else {
        Some(clamped)
    }
}

/// 指定区域（归一化坐标）的平均亮度（0-255）
pub fn region_mean_luma(
    luma: &[u8],
    w: u32,
    h: u32,
    cx: f64,
    cy: f64,
    half_w: f64,
    half_h: f64,
) -> f64 {
    let w = w as usize;
    let h = h as usize;
    if w < 2 || h < 2 || luma.len() < w * h {
        return 0.0;
    }
    let x0 = (((cx - half_w) * w as f64).round() as i64).clamp(0, w as i64 - 1) as usize;
    let x1 = (((cx + half_w) * w as f64).round() as i64).clamp(0, w as i64 - 1) as usize;
    let y0 = (((cy - half_h) * h as f64).round() as i64).clamp(0, h as i64 - 1) as usize;
    let y1 = (((cy + half_h) * h as f64).round() as i64).clamp(0, h as i64 - 1) as usize;
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in y0..=y1 {
        let row = y * w;
        for x in x0..=x1 {
            sum += luma[row + x] as u64;
            n += 1;
        }
    }
    if n == 0 {
        0.0
    } else {
        sum as f64 / n as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> ExposureCurve {
        ExposureCurve {
            target: 128.0,
            ev_full_lo: 1.0,
            ev_full_hi: 1.0,
            ev_lo: 4.0,
            ev_hi: 2.0,
            subject_blend: 0.5,
            suggest_cap: 2.0,
        }
    }

    fn stats(mean_trunc: f64) -> ExposureStats {
        ExposureStats { over_ratio: 0.0, under_ratio: 0.0, mean: mean_trunc, mean_trunc }
    }

    /// sRGB 码值 ↔ EV 的换算必须与物理一致（±1 EV ≈ 92 / 176）
    #[test]
    fn ev_offset_matches_srgb() {
        assert!(ev_offset(128.0, 128.0).abs() < 1e-9);
        assert!((ev_offset(92.0, 128.0) + 1.0).abs() < 0.02, "{}", ev_offset(92.0, 128.0));
        assert!((ev_offset(176.0, 128.0) - 1.0).abs() < 0.02, "{}", ev_offset(176.0, 128.0));
        // 码值 200 ≈ +1.44 EV
        let a = ev_offset(200.0, 128.0);
        assert!((a - 1.44).abs() < 0.05, "{a}");
    }

    /// 容差带内满分：-1 EV ~ +1 EV 之间的任何亮度都不应被扣分
    #[test]
    fn full_score_inside_ev_band() {
        let c = curve();
        for code in [93.0, 110.0, 128.0, 150.0, 175.0] {
            let s = exposure_score(&stats(code), None, &c);
            assert!((s - 100.0).abs() < 0.5, "码值 {code} 得分 {s} 应满分");
        }
    }

    /// 带外单调衰减，且在 -4 EV / +2 EV 处归零（-4 EV ≈ 码值 31，+2 EV ≈ 码值 239）
    #[test]
    fn decays_monotonically_outside_band() {
        let c = curve();
        let d2 = exposure_score(&stats(65.7), None, &c); // -2 EV
        let d3 = exposure_score(&stats(45.7), None, &c); // -3 EV
        let d4 = exposure_score(&stats(31.0), None, &c); // -4 EV
        assert!(d2 > d3 && d3 > d4, "暗侧应单调下降: {d2} {d3} {d4}");
        assert!(d4 < 3.0, "-4 EV 应接近 0 分，实际 {d4}");
        // -2 EV 落在 -1 ~ -4 EV 的线性段上：1 - (2-1)/(4-1) = 2/3 → 66.7 分
        assert!((d2 - 66.7).abs() < 2.0, "-2 EV 应约 66.7 分，实际 {d2}");
        let b15 = exposure_score(&stats(205.0), None, &c); // +1.5 EV
        let b20 = exposure_score(&stats(239.0), None, &c); // +2 EV
        assert!(b15 > b20, "亮侧应单调下降: {b15} {b20}");
        assert!(b20 < 3.0, "+2 EV 应接近 0 分，实际 {b20}");
    }

    /// 两侧容差独立：放宽暗侧不应顺带放宽亮侧（舞台/夜景的关键）
    #[test]
    fn dark_and_bright_tolerance_are_independent() {
        let mut c = curve();
        c.ev_full_lo = 2.0;
        c.ev_lo = 5.0;
        // 暗侧 -1.5 EV 现在满分
        assert!((exposure_score(&stats(78.1), None, &c) - 100.0).abs() < 0.5);
        // 亮侧 +1.5 EV 仍被扣分
        let bright = exposure_score(&stats(205.0), None, &c);
        assert!(bright < 60.0, "亮侧不应被暗侧容差放宽，实际 {bright}");
    }

    /// 主体感知：暗背景下主体脸亮 → 分数应被拉起来
    #[test]
    fn subject_blend_rescues_dark_background() {
        let c = curve();
        let dark = stats(38.0); // 舞台黑幕布把全图均值拉低
        let global_only = exposure_score(&dark, None, &c);
        let with_subject = exposure_score(&dark, Some(110.0), &c);
        assert!(global_only < 20.0, "全图判定应很低，实际 {global_only}");
        assert!(with_subject > 60.0, "主体脸正常时应救回，实际 {with_subject}");
        // 无主体脸时不得被影响
        assert!((exposure_score(&dark, None, &c) - global_only).abs() < 1e-9);
    }

    /// 主体亮度是单向修正：不能越过中灰把判定推到另一侧，也不能反向压低
    #[test]
    fn subject_blend_is_one_sided() {
        let c = curve();
        // 全图偏暗时，主体亮度高于中灰也只拉到中灰为止
        let dark = stats(50.0);
        let over_bright = exposure_score(&dark, Some(240.0), &c);
        let at_target = exposure_score(&dark, Some(128.0), &c);
        assert!((over_bright - at_target).abs() < 1e-9, "不应越过中灰");
        // 全图正常时，暗的误检脸不得把分数压低
        let normal = stats(120.0);
        assert!(
            (exposure_score(&normal, Some(20.0), &c) - exposure_score(&normal, None, &c)).abs()
                < 1e-9,
            "暗色误检框不应扣分"
        );
        // 全图偏亮（白裙场景）时，主体亮度可把判定拉回中灰方向
        let bright = stats(206.8);
        let rescued = exposure_score(&bright, Some(154.7), &c);
        assert!(rescued > 70.0, "白背景场景应被救回，实际 {rescued}");
    }

    /// 剪裁惩罚：死白比例高时即使亮度落在容差带内也要扣分
    #[test]
    fn clipping_penalty_applies() {
        let c = curve();
        let mut s = stats(128.0);
        s.over_ratio = 0.1; // 10% 死白
        let scored = exposure_score(&s, None, &c);
        assert!(scored < 70.0 && scored > 50.0, "10% 死白应扣到 ~60 分，实际 {scored}");
    }

    /// 建议 EV：容差带内不给建议（与打分同带，正常的片不折腾）
    #[test]
    fn suggested_ev_none_inside_band() {
        let c = curve();
        for code in [93.0, 110.0, 128.0, 150.0, 175.0] {
            assert!(suggested_ev(&stats(code), None, &c).is_none(), "码值 {code} 带内不应有建议");
        }
        // 边界外 1/3 档（码值 87 ≈ -1.18 EV）→ 建议 +1.33（1/3 档取整）
        let dark = suggested_ev(&stats(87.0), None, &c);
        assert!(dark.map_or(false, |v| (v - 4.0 / 3.0).abs() < 1e-9), "应建议 +1.33 档，实际 {dark:?}");
    }

    /// 建议 EV：方向与量级（暗 → +EV、亮 → −EV），1/3 档取整
    #[test]
    fn suggested_ev_direction_and_third_stop() {
        let c = curve();
        // 码值 65.7 ≈ -2 EV → 建议 +2.0
        let dark = suggested_ev(&stats(65.7), None, &c).unwrap();
        assert!((dark - 2.0).abs() < 1e-9, "欠曝 2 档应建议 +2.0，实际 {dark}");
        // 码值 239 ≈ +2 EV → 建议 -2.0
        let bright = suggested_ev(&stats(239.0), None, &c).unwrap();
        assert!((bright + 2.0).abs() < 1e-9, "过曝 2 档应建议 -2.0，实际 {bright}");
        // 1/3 档取整：码值 100 ≈ -0.76 EV，仍在容差带内（< -1 EV 才给建议）
        assert!(suggested_ev(&stats(100.0), None, &c).is_none());
        // 码值 89 ≈ -1.11 EV → +1.11 取 1/3 档 = +1.0（3/3）
        let r = suggested_ev(&stats(89.0), None, &c).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "89 码值应建议 +1.0，实际 {r}");
        // 码值 87 ≈ -1.18 EV → +1.18 取 1/3 档 = +1.33（4/3）
        let r = suggested_ev(&stats(87.0), None, &c).unwrap();
        assert!((r - 4.0 / 3.0).abs() < 1e-9, "87 码值应建议 +1.33，实际 {r}");
    }

    /// 建议 EV：超上限钳位（极欠/极曝不给出鬼建议），suggest_cap=0 关闭
    #[test]
    fn suggested_ev_clamped_by_cap() {
        // 码值 31 ≈ -4 EV，cap=2.0 → 只建议 +2.0
        let c = curve();
        let dark = suggested_ev(&stats(31.0), None, &c).unwrap();
        assert!((dark - 2.0).abs() < 1e-9, "-4 EV 应钳到 +2.0，实际 {dark}");
        // cap=0 → 一律无建议
        let c0 = ExposureCurve { suggest_cap: 0.0, ..curve() };
        assert!(suggested_ev(&stats(31.0), None, &c0).is_none());
        assert!(suggested_ev(&stats(128.0), None, &c0).is_none());
        // 小 cap：cap=0.5 → -4 EV 建议 +0.5
        let c5 = ExposureCurve { suggest_cap: 0.5, ..curve() };
        let dark = suggested_ev(&stats(31.0), None, &c5).unwrap();
        assert!((dark - 0.5).abs() < 1e-9, "实际 {dark}");
    }

    /// 建议 EV：与打分共用同一判定亮度（主体脸拉回影响建议，方向一致）
    #[test]
    fn suggested_ev_uses_subject_basis() {
        let c = curve();
        let dark = stats(38.0); // 舞台黑幕布
        let global = suggested_ev(&dark, None, &c); // -2.26 EV → +2.0（钳位）
        let with_subject = suggested_ev(&dark, Some(110.0), &c); // 判定被拉回 → 建议变小
        assert!(global.is_some() && with_subject.is_some());
        assert!(with_subject.unwrap() < global.unwrap(), "主体脸救回后建议应减小");
    }
}
