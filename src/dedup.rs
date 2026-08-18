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
    Some(days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss)
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

/// 按拍摄时间聚类，返回每个 JPG 条目的组号（0 = 非连拍，1 起为连拍组）
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
                gid += 1; // 无时间戳的单独成组（非连拍）
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
            order.sort_by(|&a, &b| scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal));
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
