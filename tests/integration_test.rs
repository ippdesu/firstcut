//! 集成测试：端到端 pipeline 验证（扫描 → 评分 → 输出）
//!
//! 使用项目本地的 testpic/ 目录中的真实照片进行测试；
//! testpic 不入库（gitignore），缺失时跳过依赖它的用例（打印提示），
//! 其余纯逻辑用例（配置/去重/评分/星级/模板）始终执行。

use pic_process::config::{DedupParams, ScoreConfig};
use pic_process::dedup;
use pic_process::scan;
use pic_process::score;
use pic_process::output;
use std::path::Path;

/// 验证扫描结果包含预期的照片文件（依赖本地 testpic，缺失时跳过）
#[test]
fn test_scan_directory_finds_photos() {
    let dir = Path::new("testpic");
    if !dir.exists() {
        eprintln!("testpic 不存在，跳过 test_scan_directory_finds_photos（克隆环境无真实照片）");
        return;
    }
    let entries = scan::scan_directory(dir).expect("scan_directory 应成功");
    // 应该至少找到 testpic/JPG 和 testpic/RAW 中的照片
    assert!(entries.len() >= 2, "至少应找到 2 张照片，实际找到 {}", entries.len());

    // 检查 JPG 和 ARW 都有
    let jpg_count = entries.iter().filter(|e| e.extension == "jpg").count();
    let arw_count = entries.iter().filter(|e| e.extension == "arw").count();
    assert!(jpg_count > 0, "应找到 JPG 文件");
    assert!(arw_count > 0, "应找到 ARW 文件");

    // 检查部分有配对的 JPG（如 DSC00886 有 ARW，PORTRAIT_TEST 没有）
    let paired_count = entries.iter().filter(|e| e.has_pair).count();
    assert!(paired_count > 0, "应有有配对的 JPG/ARW 对");
    // 也应有没有配对的（PORTRAIT_TEST.JPG）
    let unpaired = entries.iter().filter(|e| !e.has_pair).count();
    assert!(unpaired > 0, "应有无配对的文件（如 PORTRAIT_TEST）");
}

/// 验证配置加载和权重校验
#[test]
fn test_config_validation() {
    // 默认权重和应为 1.0（清晰度 0.30 + 曝光 0.25 + 噪点 0.15 + 构图 0.15 + 美学 0.15）
    let default_cfg = ScoreConfig::default();
    let w = &default_cfg.weights;
    let sum = w.sharpness + w.exposure + w.noise + w.composition + w.aesthetic;
    // 与 load_config 的校验一致：偏离 1.0 超过 0.05 会被拒绝
    assert!(sum >= 0.95 && sum <= 1.05, "默认权重和应在 0.95~1.05 范围内，实际为 {}", sum);

    // 验证曝光权重为 0.25（M5 决策 A）
    assert_eq!(w.exposure, 0.25, "曝光权重应为 0.25");
}

/// 验证连拍去重与评分汇总的协同工作
#[test]
fn test_dedup_integration_with_scores() {
    // 模拟 5 张连拍照片（按时间顺序，最后一张间隔 20s 非连拍）
    let entries = vec![
        scan::PhotoEntry {
            path: "testpic/JPG/A.JPG".into(),
            filename: "A.JPG".into(),
            extension: "jpg".into(),
            is_raw: false,
            has_pair: false,
            date_time_original: "2026:07:19 17:12:38".into(),
            iso: "100".into(),
            ..Default::default()
        },
        scan::PhotoEntry {
            path: "testpic/JPG/B.JPG".into(),
            filename: "B.JPG".into(),
            extension: "jpg".into(),
            is_raw: false,
            has_pair: false,
            date_time_original: "2026:07:19 17:12:39".into(),
            iso: "100".into(),
            ..Default::default()
        },
        scan::PhotoEntry {
            path: "testpic/JPG/C.JPG".into(),
            filename: "C.JPG".into(),
            extension: "jpg".into(),
            is_raw: false,
            has_pair: false,
            date_time_original: "2026:07:19 17:12:40".into(),
            iso: "100".into(),
            ..Default::default()
        },
        scan::PhotoEntry {
            path: "testpic/JPG/E.JPG".into(),
            filename: "E.JPG".into(),
            extension: "jpg".into(),
            is_raw: false,
            has_pair: false,
            date_time_original: "2026:07:19 17:12:40".into(),
            iso: "100".into(),
            ..Default::default()
        },
        scan::PhotoEntry {
            path: "testpic/JPG/D.JPG".into(),
            filename: "D.JPG".into(),
            extension: "jpg".into(),
            is_raw: false,
            has_pair: false,
            date_time_original: "2026:07:19 17:13:00".into(), // 与 E 间隔 20s，非连拍
            iso: "100".into(),
            ..Default::default()
        },
    ];

    // 相同 dHash（同一场景）；无姿态描述子（伪簇，行为与 M2 一致）
    let hashes = vec![0xAAAA_AAAA_AAAA_AAAAu64; 5];
    // 分数：B > C > A > E（D 不在连拍组）
    let scores: Vec<f64> = vec![70.0, 90.0, 80.0, 65.0, 60.0];
    let descs: Vec<Option<[f32; 10]>> = vec![None; 5];

    // keep_k=2 + 关闭自适应保留：锁定 M2 语义回归
    let params = DedupParams { keep_k: 2, adaptive_keep: false, ..Default::default() };
    let infos = dedup::analyze_bursts(&entries, &hashes, &scores, &descs, &params);

    // 验证连拍分组：A、B、C、E 应属于同一组（时间间隔 ≤ 2s），D 应单独成组（后被单成员退化为 0）
    let burst_group_id = infos[0].group;
    assert!(burst_group_id > 0, "A 应属于连拍组（组号 {}）", burst_group_id);
    assert_eq!(infos[1].group, burst_group_id, "B 应与 A 同组");
    assert_eq!(infos[2].group, burst_group_id, "C 应与 A 同组");
    assert_eq!(infos[3].group, burst_group_id, "E 应与 A 同组");
    // D 是单成员组（组号 > burst_group_id），会被退化为 0
    assert_eq!(infos[4].group, 0, "D 应不属于连拍组（单成员退化）");

    // 验证排名：B(90) 第 1, C(80) 第 2, A(70) 第 3, E(65) 第 4
    assert_eq!(infos[1].rank, 1, "B 应排名第 1");
    assert_eq!(infos[2].rank, 2, "C 应排名第 2");

    // 验证 keep（keep_k=2）：B 和 C 保留
    assert!(infos[1].keep, "B 应保留");
    assert!(infos[2].keep, "C 应保留");
    assert!(!infos[0].keep, "A 不应保留");
    assert!(!infos[3].keep, "E 不应保留");
}

/// 验证加权总分计算（使用更新后的曝光权重 0.25）
#[test]
fn test_total_score_with_updated_weights() {
    let scores = score::PixelScores {
        sharpness: 80.0,
        exposure: 70.0,
        noise: 60.0,
        composition: 75.0,
        aesthetic: 65.0,
    };
    let cfg = ScoreConfig::default();

    let total = score::total_score(&scores, &cfg.weights);

    // 预期（M5 决策 A 修复后：清晰 0.30 + 曝光 0.25，权重和 = 1.0）：
    // 80*0.30 + 70*0.25 + 60*0.15 + 75*0.15 + 65*0.15 = 24 + 17.5 + 9 + 11.25 + 9.75 = 71.5
    let expected: f64 = 80.0 * 0.30 + 70.0 * 0.25 + 60.0 * 0.15 + 75.0 * 0.15 + 65.0 * 0.15;
    let expected_rounded = (expected * 10.0).round() / 10.0;

    assert!((total - expected_rounded).abs() < 0.01,
        "总分应为 {:.1}，实际为 {:.1}", expected_rounded, total);
}

/// 验证 XMP 星级映射（M5 决策 B2：放宽分档 75/60/45/30）
#[test]
fn test_rating_mapping_relaxed_thresholds() {
    let m = pic_process::config::MetricParams {
        rating_5: 75.0,
        rating_4: 60.0,
        rating_3: 45.0,
        rating_2: 30.0,
        ..Default::default()
    };

    // ≥75 应为 5 星
    assert_eq!(output::xmp::rating_from_total(75.0, &m), 5);
    assert_eq!(output::xmp::rating_from_total(90.0, &m), 5);

    // 60-74.9 应为 4 星
    assert_eq!(output::xmp::rating_from_total(60.0, &m), 4);
    assert_eq!(output::xmp::rating_from_total(74.9, &m), 4);

    // 45-59.9 应为 3 星
    assert_eq!(output::xmp::rating_from_total(45.0, &m), 3);
    assert_eq!(output::xmp::rating_from_total(59.9, &m), 3);

    // 30-44.9 应为 2 星
    assert_eq!(output::xmp::rating_from_total(30.0, &m), 2);
    assert_eq!(output::xmp::rating_from_total(44.9, &m), 2);

    // <30 应为 1 星
    assert_eq!(output::xmp::rating_from_total(29.9, &m), 1);
    assert_eq!(output::xmp::rating_from_total(0.0, &m), 1);
}

/// 验证配置模板生成
#[test]
fn test_config_template_contains_all_fields() {
    let template = pic_process::config::config_template();

    // 模板应包含所有权重字段
    assert!(template.contains("[weights]"), "模板应包含 [weights] 节");
    assert!(template.contains("sharpness"), "模板应包含 sharpness 字段");
    assert!(template.contains("exposure ="), "模板应包含 exposure 字段");

    // 模板应包含 metric 节
    assert!(template.contains("[metric]"), "模板应包含 [metric] 节");
    assert!(template.contains("rating_5"), "模板应包含 rating_5 字段");
    assert!(template.contains("rating_4"), "模板应包含 rating_4 字段");

    // 模板应包含注释说明
    assert!(template.contains("#"), "模板应包含注释");
    assert!(template.contains("人像") || template.contains("打鸟") || template.contains("夜景"),
        "模板应包含场景示例说明");
}
