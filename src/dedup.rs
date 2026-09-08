//! 连拍去重/最优帧选择（M2/M9）
//!
//! 三步聚类（M9 起自适应保留默认开启）：
//! 1. 时间聚类：EXIF 拍摄时间间隔 ≤ 阈值（默认 2s）视为同一连拍组
//! 2. 组内感知相似聚类：dHash 汉明距离 ≤ 阈值视为同一场景子簇
//!    （长连拍中画面渐变时，避免首尾被错误归为同帧）
//! 3. **保留单元**内按总分排序，top-K 标记保留：
//!    - `adaptive_keep = true`（M9，默认）：在 dHash 子簇内再按 SCRFD 关键点
//!      姿态描述子聚类（距离 > 阈值 = 不同姿势），每个**姿势簇**各自保留 top-K，
//!      解决"30fps 连拍不同姿势被 dHash 判为近似重复"的问题；
//!      单组保留总量受 `burst_group_cap` 上限约束（超出按总分截断）。
//!    - `adaptive_keep = false`：保留单元退化为 dHash 子簇（M2 行为）。

use crate::config::DedupParams;
use crate::scan::PhotoEntry;

/// 连拍组信息（按 JPG 条目的顺序索引）
#[derive(Debug, Clone)]
pub struct BurstInfo {
    /// 连拍组号（0 = 非连拍）
    pub group: usize,
    /// 保留单元内张数（姿态簇或 dHash 子簇）
    pub size: usize,
    /// 保留单元内排名（1 = 最优）
    pub rank: usize,
    /// 是否建议保留
    pub keep: bool,
    /// M9 姿态簇号（组内从 1 起；0 = 未启用自适应保留或非连拍）。
    /// 不同 dHash 子簇间簇号可能重复（同姿势出现在不同子簇是正常的）。
    pub pose_cluster: usize,
}

/// 姿态描述子欧氏距离（10 维：5 个关键点 × (x,y)，已按人脸框归一化）
pub fn pose_distance(a: &[f32; 10], b: &[f32; 10]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = (*x - *y) as f64;
            d * d
        })
        .sum::<f64>()
        .sqrt()
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

/// 连拍分析：输入 JPG 条目（含分数、dHash 与姿态描述子），输出每条目的 BurstInfo
///
/// `descs[i]` 为 `entries[i]` 的姿态描述子（无主体级人脸时 None）。
pub fn analyze_bursts(
    entries: &[PhotoEntry],
    hashes: &[u64],
    scores: &[f64],
    descs: &[Option<[f32; 10]>],
    params: &DedupParams,
) -> Vec<BurstInfo> {
    assert_eq!(entries.len(), hashes.len(), "hashes 与 entries 长度不一致");
    assert_eq!(entries.len(), scores.len(), "scores 与 entries 长度不一致");
    assert_eq!(entries.len(), descs.len(), "descs 与 entries 长度不一致");
    let n = entries.len();
    let times: Vec<Option<i64>> = entries
        .iter()
        .map(|e| parse_datetime(&e.date_time_original))
        .collect();
    let groups = group_by_time(&times, params.gap_secs);

    let mut infos = vec![
        BurstInfo { group: 0, size: 0, rank: 0, keep: false, pose_cluster: 0 };
        n
    ];

    let max_group = groups.iter().copied().max().unwrap_or(0);
    for gid in 1..=max_group {
        let members: Vec<usize> = (0..n).filter(|&i| groups[i] == gid).collect();
        if members.len() < 2 {
            for &i in &members {
                infos[i] =
                    BurstInfo { group: 0, size: 0, rank: 0, keep: false, pose_cluster: 0 };
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
        // 每个子簇内确定保留单元（姿态簇或子簇本身），排序定排名与保留
        for c in 1..=cid {
            let in_cluster: Vec<usize> = (0..members.len())
                .filter(|&mi| cluster_of[mi] == c)
                .map(|mi| members[mi])
                .collect();
            if !params.adaptive_keep {
                assign_rank_keep(&mut infos, &in_cluster, scores, gid, params.keep_k, 0);
                continue;
            }
            // M9：dHash 子簇内按姿态描述子贪心种子聚类。
            // 与 dHash 聚类同风格（对簇种子比较），扫描顺序确定，结果可复现。
            let mut pose_seeds: Vec<[f32; 10]> = Vec::new();
            let mut pose_of = vec![0usize; in_cluster.len()];
            let mut no_desc: Vec<usize> = Vec::new();
            for (mi, &idx) in in_cluster.iter().enumerate() {
                match &descs[idx] {
                    Some(d) => {
                        let hit = pose_seeds
                            .iter()
                            .position(|s| pose_distance(s, d) <= params.pose_cluster_threshold);
                        match hit {
                            Some(ci) => pose_of[mi] = ci + 1,
                            None => {
                                pose_seeds.push(*d);
                                pose_of[mi] = pose_seeds.len();
                            }
                        }
                    }
                    None => no_desc.push(mi),
                }
            }
            // 无描述子的帧（无人脸/关键点退化）共享一个伪簇，编号接在后面：
            // 它们已被 dHash 判为近似同帧，合并处理与 M2 行为等价
            let pseudo_id = pose_seeds.len() + 1;
            for &mi in &no_desc {
                pose_of[mi] = pseudo_id;
            }
            for pc in 1..=pseudo_id {
                let unit: Vec<usize> = (0..in_cluster.len())
                    .filter(|&mi| pose_of[mi] == pc)
                    .map(|mi| in_cluster[mi])
                    .collect();
                assign_rank_keep(&mut infos, &unit, scores, gid, params.keep_k, pc);
            }
        }
        // M9：单组保留总量上限——超出部分按总分从低到高截断（排名不变）
        if params.adaptive_keep && params.burst_group_cap > 0 {
            let kept: Vec<usize> =
                (0..n).filter(|&i| infos[i].group == gid && infos[i].keep).collect();
            if kept.len() > params.burst_group_cap {
                let mut by_score_asc = kept;
                by_score_asc.sort_by(|&a, &b| {
                    scores[a].partial_cmp(&scores[b]).unwrap_or(std::cmp::Ordering::Equal)
                });
                let excess = by_score_asc.len() - params.burst_group_cap;
                for &idx in by_score_asc.iter().take(excess) {
                    infos[idx].keep = false;
                }
            }
        }
    }
    infos
}

/// 给一个保留单元内的条目按总分降序定排名与保留标记
fn assign_rank_keep(
    infos: &mut [BurstInfo],
    unit: &[usize],
    scores: &[f64],
    gid: usize,
    keep_k: usize,
    pose_cluster: usize,
) {
    let mut order = unit.to_vec();
    order.sort_by(|&a, &b| {
        scores[b].partial_cmp(&scores[a]).unwrap_or(std::cmp::Ordering::Equal)
    });
    let size = order.len();
    for (rank, &idx) in order.iter().enumerate() {
        infos[idx] = BurstInfo {
            group: gid,
            size,
            rank: rank + 1,
            keep: rank < keep_k,
            pose_cluster,
        };
    }
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
            pair_id: name.to_string(),
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
            composition_score: String::new(),
            aesthetic_score: String::new(),
            total_score: String::new(),
            stars: String::new(),
            faces: String::new(),
            analysis_ok: String::new(),
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

        // 严格递减图案（每对相邻列左>右）vs 全灰：所有比较位相反 → 距离 64
        let mut luma3 = vec![128u8; 9 * 8];
        for y in 0..8 {
            for x in 0..9 {
                luma3[y * 9 + x] = 255 - (x as u8) * 28; // 255,227,199,...,31 严格递减
            }
        }
        let h3 = dhash(&luma3, 9, 8);
        assert_eq!(h3, u64::MAX, "严格递减应产生全 1 哈希");
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
        let descs = vec![None; 4];
        let params = DedupParams { keep_k: 2, adaptive_keep: false, ..Default::default() };
        let infos = analyze_bursts(&entries, &hashes, &scores, &descs, &params);

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
        // 自适应保留关闭：姿态簇号为 0
        assert_eq!(infos[0].pose_cluster, 0);
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
        let descs = vec![None, None];
        let params = DedupParams { adaptive_keep: false, ..Default::default() };
        let infos = analyze_bursts(&entries, &hashes, &scores, &descs, &params);
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
        let descs = vec![None, None];
        let infos = analyze_bursts(&entries, &hashes, &scores, &descs, &DedupParams::default());
        assert_eq!(infos[0].group, 0);
        assert_eq!(infos[1].group, 0);
    }

    #[test]
    fn no_desc_frames_behave_like_m2() {
        // 自适应开启但全部无描述子 → 共享伪簇，保留行为与 M2 等价
        let entries = vec![
            entry("A.JPG", "2026:07:19 17:12:38"),
            entry("B.JPG", "2026:07:19 17:12:39"),
            entry("C.JPG", "2026:07:19 17:12:40"),
        ];
        let hashes = vec![0xAAAA_AAAA_AAAA_AAAAu64; 3];
        let scores = vec![50.0, 90.0, 70.0];
        let descs = vec![None, None, None];
        let infos = analyze_bursts(&entries, &hashes, &scores, &descs, &DedupParams::default());
        assert_eq!(infos[1].rank, 1);
        assert_eq!(infos[2].rank, 2);
        assert_eq!(infos[0].rank, 3);
        assert!((infos[0].pose_cluster, infos[1].pose_cluster, infos[2].pose_cluster) == (1, 1, 1));
        assert!(infos[0].keep && infos[1].keep && infos[2].keep);
    }

    // ---- M9 姿态聚类 ----

    /// 合成描述子：10 维基向量，鼻子位（4,5）可偏移制造"不同姿势"
    fn pose_desc(nose_dx: f32, nose_dy: f32) -> [f32; 10] {
        let mut d = [0.5f32, 0.4, 0.3, 0.2, 0.5, 0.6, 0.4, 0.7, 0.6, 0.8];
        d[4] += nose_dx;
        d[5] += nose_dy;
        d
    }

    #[test]
    fn pose_distance_known_values() {
        let a = pose_desc(0.0, 0.0);
        assert_eq!(pose_distance(&a, &a), 0.0, "相同描述子距离为 0");
        let b = pose_desc(0.4, 0.4);
        // √(0.4² + 0.4²) ≈ 0.5657（f32 描述子，容差 1e-4）
        assert!((pose_distance(&a, &b) - ((0.4f32 * 0.4 + 0.4 * 0.4) as f64).sqrt()).abs() < 1e-4);
    }

    #[test]
    fn adaptive_clustering_keeps_each_pose() {
        // 4 帧同一 dHash 子簇，两个姿势各 2 帧：
        // 旧行为（单保留单元 keep_k=3）会丢掉第 4 名，M9 每个姿势簇各自保留
        let entries = vec![
            entry("A.JPG", "2026:07:19 17:12:38"),
            entry("B.JPG", "2026:07:19 17:12:39"),
            entry("C.JPG", "2026:07:19 17:12:39"),
            entry("D.JPG", "2026:07:19 17:12:40"),
        ];
        let hashes = vec![0xAAAA_AAAA_AAAA_AAAAu64; 4];
        let scores = vec![70.0, 90.0, 80.0, 60.0];
        // A、B 同姿势；C、D 另一姿势（与前者距离 0.566 > 阈值 0.25）
        let descs = vec![
            Some(pose_desc(0.0, 0.0)),
            Some(pose_desc(0.02, -0.02)),
            Some(pose_desc(0.4, 0.4)),
            Some(pose_desc(0.38, 0.42)),
        ];
        let infos = analyze_bursts(&entries, &hashes, &scores, &descs, &DedupParams::default());

        // 姿态分簇：{A,B} 一簇，{C,D} 一簇
        assert_eq!(infos[0].pose_cluster, infos[1].pose_cluster);
        assert_eq!(infos[2].pose_cluster, infos[3].pose_cluster);
        assert_ne!(infos[0].pose_cluster, infos[2].pose_cluster);
        // 每个姿势簇内 top-3 全保留（簇内只有 2 张）→ 4 张全保留；
        // 旧行为下 D(60) 是 4 名中最低分会被丢掉
        assert!(infos.iter().all(|i| i.keep), "不同姿势都应保留: {infos:?}");
        // 簇内排名：B(90) 在 {A,B} 第一，C(80) 在 {C,D} 第一
        assert_eq!(infos[1].rank, 1);
        assert_eq!(infos[0].rank, 2);
        assert_eq!(infos[2].rank, 1);
        assert_eq!(infos[3].rank, 2);
        // 关闭自适应 → 回到旧行为：单保留单元 top-3，D 被丢
        let params = DedupParams { adaptive_keep: false, ..Default::default() };
        let old = analyze_bursts(&entries, &hashes, &scores, &descs, &params);
        assert!(!old[3].keep, "旧行为应丢弃最低分");
        assert!(old[0].keep && old[1].keep && old[2].keep);
    }

    #[test]
    fn pose_threshold_groups_by_seed_distance() {
        // 贪心种子聚类：与簇种子距离 ≤ 阈值入簇，否则新簇
        // d1(种子) ← d2(0.17，入簇)；d3 距 d1 0.57 > 0.25 → 新簇
        let entries = vec![
            entry("A.JPG", "2026:07:19 17:12:38"),
            entry("B.JPG", "2026:07:19 17:12:39"),
            entry("C.JPG", "2026:07:19 17:12:40"),
        ];
        let hashes = vec![0xAAAA_AAAA_AAAA_AAAAu64; 3];
        let scores = vec![70.0, 80.0, 90.0];
        let descs = vec![
            Some(pose_desc(0.0, 0.0)),
            Some(pose_desc(0.12, 0.12)),
            Some(pose_desc(0.4, 0.4)),
        ];
        let infos = analyze_bursts(&entries, &hashes, &scores, &descs, &DedupParams::default());
        assert_eq!(infos[0].pose_cluster, infos[1].pose_cluster);
        assert_ne!(infos[0].pose_cluster, infos[2].pose_cluster);
    }

    #[test]
    fn group_cap_truncates_lowest_scores() {
        // 同一姿势簇 5 帧（keep_k=3 → 3 张保留），组上限 2 → 再按总分截掉 1 张
        let entries = vec![
            entry("A.JPG", "2026:07:19 17:12:38"),
            entry("B.JPG", "2026:07:19 17:12:39"),
            entry("C.JPG", "2026:07:19 17:12:40"),
            entry("D.JPG", "2026:07:19 17:12:41"),
            entry("E.JPG", "2026:07:19 17:12:42"),
        ];
        let hashes = vec![0xAAAA_AAAA_AAAA_AAAAu64; 5];
        let scores = vec![50.0, 90.0, 70.0, 60.0, 80.0];
        let d = pose_desc(0.0, 0.0);
        let descs = vec![Some(d), Some(d), Some(d), Some(d), Some(d)];
        let params =
            DedupParams { keep_k: 3, burst_group_cap: 2, ..Default::default() };
        let infos = analyze_bursts(&entries, &hashes, &scores, &descs, &params);

        // 排名不变：B(90) 1、E(80) 2、C(70) 3、D(60) 4、A(50) 5
        assert_eq!(infos[1].rank, 1);
        assert_eq!(infos[4].rank, 2);
        assert_eq!(infos[2].rank, 3);
        // 上限 2：只保留总分前 2，第 3 名被截断但排名保留
        assert!(infos[1].keep && infos[4].keep);
        assert!(!infos[2].keep && !infos[3].keep && !infos[0].keep);
        assert_eq!(infos.iter().filter(|i| i.keep).count(), 2);
    }
}
