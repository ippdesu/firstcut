//! 连拍去重/最优帧选择（M2）
//!
//! 两步聚类：
//! 1. 时间聚类：EXIF 拍摄时间间隔 ≤ 阈值（默认 2s）视为同一连拍组
//! 2. 组内感知相似聚类：dHash 汉明距离 ≤ 阈值视为同一场景子簇
//!    （长连拍中画面渐变时，避免首尾被错误归为同帧）
//! 3. 子簇内按总分排序，top-K 标记保留

use crate::config::DedupParams;
use crate::scan::PhotoEntry;

/// 连拍组信息（按 JPG 条目的顺序索引）
#[derive(Debug, Clone)]
pub struct BurstInfo {
    /// 连拍组号（0 = 非连拍）
    pub group: usize,
    /// 组内照片数
    pub size: usize,
    /// 子簇内排名（1 = 最优）
    pub rank: usize,
    /// 是否建议保留
    pub keep: bool,
}

/// 解析 EXIF 拍摄时间为 Unix 秒（无时区，相对比较用）
///
/// 支持 "YYYY:MM:DD HH:MM:SS" 与 "YYYY-MM-DD HH:MM:SS"（部分软件改写为 '-'）。
/// 严格校验字段范围（月 1-12、日按当月天数、时分秒 0-59）。
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once(|c| c == ' ' || c == 'T')?;
    let mut date_parts = date.split(|c| c == ':' || c == '-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let m: i64 = date_parts.next()?.parse().ok()?;
    let d: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hh: i64 = time_parts.next()?.parse().ok()?;
    let mm: i64 = time_parts.next()?.parse().ok()?;
    let ss: i64 = time_parts.next().unwrap_or("0").parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    let days = days_in_month(y, m);
    if !(1..=days).contains(&d) || !(0..24).contains(&hh) || !(0..60).contains(&mm) || !(0..60).contains(&ss)
    {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss)
}

/// 当月天数（含闰年）
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant 的 days_from_civil 算法
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// dHash（差分感知哈希）：缩到 9x8 灰度，比较水平相邻像素，得 64 位
pub fn dhash(luma: &[u8], w: u32, h: u32) -> u64 {
    if w < 2 || h < 2 {
        return 0;
    }
    let mut small = [0u8; 72];
    for gy in 0..8u64 {
        for gx in 0..9u64 {
            let x0 = gx * w as u64 / 9;
            let x1 = ((gx + 1) * w as u64 / 9).max(x0 + 1);
            let y0 = gy * h as u64 / 8;
            let y1 = ((gy + 1) * h as u64 / 8).max(y0 + 1);
            let mut sum = 0u64;
            let mut n = 0u64;
            for y in y0..y1 {
                let row = (y * w as u64) as usize;
                for x in x0..x1 {
                    sum += luma[row + x as usize] as u64;
                    n += 1;
                }
            }
            small[(gy * 9 + gx) as usize] = if n > 0 { (sum / n) as u8 } else { 0 };
        }
    }
    let mut hash = 0u64;
    for gy in 0..8u64 {
        for gx in 0..8u64 {
            let i = (gy * 9 + gx) as usize;
            if small[i] > small[i + 1] {
                hash |= 1 << (gy * 8 + gx);
            }
        }
    }
    hash
}

/// 汉明距离
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 按拍摄时间聚类，返回每个条目的组号（0 = 非连拍，1 起为连拍组）
fn group_by_time(times: &[Option<i64>], gap_secs: f64) -> Vec<usize> {
    let n = times.len();
    let mut groups = vec![0usize; n];
    let mut gid = 0usize;
    let mut prev: Option<i64> = None;
    for i in 0..n {
        match times[i] {
            Some(t) => {
                if let Some(p) = prev {
                    if (t - p) as f64 <= gap_secs {
                        groups[i] = gid;
                    } else {
                        gid += 1;
                        groups[i] = gid;
                    }
                } else {
                    gid += 1;
                    groups[i] = gid;
                }
                prev = Some(t);
            }
            None => {
                prev = None;
                groups[i] = 0;
            }
        }
    }
    // 只有一张的组退化为 0
    let mut counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &g in &groups {
        if g != 0 {
            *counts.entry(g).or_insert(0) += 1;
        }
    }
    for g in groups.iter_mut() {
        if *g != 0 && counts.get(g) == Some(&1) {
            *g = 0;
        }
    }
    groups
}

/// 连拍分析：输入 JPG 条目（含分数与 dHash），输出每条目的 BurstInfo
pub fn analyze_bursts(
    entries: &[PhotoEntry],
    hashes: &[u64],
    scores: &[f64],
    params: &DedupParams,
) -> Vec<BurstInfo> {
    let n = entries.len();
    let times: Vec<Option<i64>> = entries
        .iter()
        .map(|e| parse_datetime(&e.date_time_original))
        .collect();
    let groups = group_by_time(&times, params.gap_secs);

    let mut infos = vec![
        BurstInfo { group: 0, size: 0, rank: 0, keep: false };
        n
    ];

    let max_group = groups.iter().copied().max().unwrap_or(0);
    for gid in 1..=max_group {
        let members: Vec<usize> = (0..n).filter(|&i| groups[i] == gid).collect();
        if members.len() < 2 {
            for &i in &members {
                infos[i] = BurstInfo { group: 0, size: 0, rank: 0, keep: false };
            }
            continue;
        }
        // 组内按 dHash 种子聚类（子簇）
        let mut cluster_of = vec![0usize; members.len()];
        let mut cid = 0usize;
        for (mi, &idx) in members.iter().enumerate() {
            if cluster_of[mi] != 0 {
                continue;
            }
            cid += 1;
            cluster_of[mi] = cid;
            for (mj, &jdx) in members.iter().enumerate().skip(mi + 1) {
                if cluster_of[mj] == 0 && hamming(hashes[idx], hashes[jdx]) <= params.dhash_threshold
                {
                    cluster_of[mj] = cid;
                }
            }
        }
        // 每个子簇内按总分排序定排名与保留
        for c in 1..=cid {
            let in_cluster: Vec<usize> = (0..members.len())
                .filter(|&mi| cluster_of[mi] == c)
                .map(|mi| members[mi])
                .collect();
            let mut order = in_cluster.clone();
            order.sort_by(|&a, &b| {
                scores[b]
                    .partial_cmp(&scores[a])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let size = order.len();
            for (rank, &idx) in order.iter().enumerate() {
                infos[idx] = BurstInfo {
                    group: gid,
                    size,
                    rank: rank + 1,
                    keep: rank < params.keep_k,
                };
            }
        }
    }
    infos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, datetime: &str) -> PhotoEntry {
        PhotoEntry {
            path: format!("testpic/JPG/{name}"),
            filename: name.to_string(),
            extension: "jpg".to_string(),
            is_raw: false,
            has_pair: false,
            date_time_original: datetime.to_string(),
            camera_make: String::new(),
            camera_model: String::new(),
            lens_model: String::new(),
            iso: "100".to_string(),
            f_number: String::new(),
            shutter_speed: String::new(),
            focal_length: String::new(),
            sharpness_score: String::new(),
            exposure_score: String::new(),
            noise_score: String::new(),
            total_score: String::new(),
            burst_group: String::new(),
            burst_size: String::new(),
            burst_rank: String::new(),
            burst_keep: String::new(),
        }
    }

    #[test]
    fn parse_datetime_both_separators() {
        let a = parse_datetime("2026:07:19 13:56:32");
        let b = parse_datetime("2026-07-19 13:56:32");
        assert_eq!(a, b);
        assert!(a.is_some());
        // 2026-07-19 13:56:32 对应 epoch 1784469392（与本机交叉验证，UTC+8 时区不受影响）
        assert_eq!(a.unwrap(), 1784469392i64);
    }

    #[test]
    fn parse_datetime_invalid() {
        assert_eq!(parse_datetime(""), None);
        assert_eq!(parse_datetime("not a date"), None);
        assert_eq!(parse_datetime("2026:13:99 99:99:99"), None);
        assert_eq!(parse_datetime("2026:02:30 10:00:00"), None, "2 月无 30 日");
        assert_eq!(parse_datetime("2024:02:29 10:00:00").is_some(), true, "闰年 2/29 合法");
        assert_eq!(parse_datetime("2026:02:29 10:00:00"), None, "平年 2/29 非法");
        assert_eq!(parse_datetime("2026:07:19 24:00:00"), None, "24 时非法");
    }

    #[test]
    fn dhash_similar_images_close() {
        // 全灰图 vs 整体略变亮的全灰图：差分哈希不变
        let luma1 = vec![128u8; 9 * 8];
        let luma2 = vec![130u8; 9 * 8];
        let h1 = dhash(&luma1, 9, 8);
        let h2 = dhash(&luma2, 9, 8);
        assert_eq!(h1, h2);
        assert_eq!(hamming(h1, h2), 0);

        // 高低交替图案（每对相邻列左>右）vs 全灰：所有比较位相反 → 距离 64
        let mut luma3 = vec![128u8; 9 * 8];
        for y in 0..8 {
            for x in 0..9 {
                luma3[y * 9 + x] = if x % 2 == 0 { 200 } else { 50 };
            }
        }
        let h3 = dhash(&luma3, 9, 8);
        assert_eq!(h3, u64::MAX, "高低交替应产生全 1 哈希");
        assert_eq!(hamming(h1, h3), 64);
    }

    #[test]
    fn burst_grouping_and_ranking() {
        let entries = vec![
            entry("A.JPG", "2026:07:19 17:12:38"),
            entry("B.JPG", "2026:07:19 17:12:39"), // 与 A 间隔 1s → 同组
            entry("C.JPG", "2026:07:19 17:12:40"), // 与 B 间隔 1s → 同组
            entry("D.JPG", "2026:07:19 17:13:00"), // 与 C 间隔 20s → 非连拍
        ];
        // 相同 dHash（同一场景），分数 B > A > C
        let hashes = vec![0xAAAA_AAAA_AAAA_AAAAu64; 4];
        let scores = vec![50.0, 90.0, 70.0, 60.0];
        let infos = analyze_bursts(&entries, &hashes, &scores, &DedupParams::default());

        assert_eq!(infos[0].group, infos[1].group);
        assert_eq!(infos[0].group, infos[2].group);
        assert_eq!(infos[3].group, 0, "间隔过大的照片不应成组");
        assert_eq!(infos[0].size, 3);
        // 组内按分数排序：B(90) 第一，C(70) 第二，A(50) 第三
        assert_eq!(infos[1].rank, 1);
        assert_eq!(infos[2].rank, 2);
        assert_eq!(infos[0].rank, 3);
        // keep_k=2：B、C 保留，A 不保留
        assert!(infos[1].keep);
        assert!(infos[2].keep);
        assert!(!infos[0].keep);
    }

    #[test]
    fn burst_splits_on_dhash_difference() {
        // 同一时间组内画面差异大 → 分成不同子簇，各自排名从 1 开始
        let entries = vec![
            entry("A.JPG", "2026:07:19 17:12:40"),
            entry("B.JPG", "2026:07:19 17:12:41"),
        ];
        let hashes = vec![0x0000_0000_0000_0000u64, 0xFFFF_FFFF_FFFF_FFFFu64];
        let scores = vec![70.0, 80.0];
        let infos = analyze_bursts(&entries, &hashes, &scores, &DedupParams::default());
        // 两个子簇，各自 rank=1 且 keep
        assert_eq!(infos[0].group, infos[1].group);
        assert_eq!(infos[0].rank, 1);
        assert_eq!(infos[1].rank, 1);
        assert!(infos[0].keep);
        assert!(infos[1].keep);
    }

    #[test]
    fn missing_datetime_no_burst() {
        let entries = vec![entry("A.JPG", ""), entry("B.JPG", "")];
        let hashes = vec![0xAAAA_AAAA_AAAA_AAAAu64; 2];
        let scores = vec![70.0, 80.0];
        let infos = analyze_bursts(&entries, &hashes, &scores, &DedupParams::default());
        assert_eq!(infos[0].group, 0);
        assert_eq!(infos[1].group, 0);
    }
}
