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

/// 人工决定贯穿重跑、CSV、XMP 与复核快照；不依赖私人 testpic。
#[test]
fn manual_rating_survives_rescore_and_export() {
    let root = std::env::temp_dir().join(format!("firstcut_flow_{}_{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&root).unwrap();
    let jpg = root.join("sample.jpg");
    image::RgbImage::from_pixel(64, 64, image::Rgb([100, 120, 140])).save(&jpg).unwrap();
    let csv_path = root.join("report.csv");
    let cache_path = root.join("cache.sqlite");
    let cfg = ScoreConfig::default();
    let opts = score::ScoreJobOptions {
        output_csv: csv_path.clone(), raw_csv: false, cache_path: cache_path.clone(), no_cache: false,
        no_ai: true, xmp: true, xmp_exposure: false, gpu: false, keep_override: None,
        include_raw: true, skip_processed: false,
    };
    score::run_score_job(&root, &cfg, &opts, &|_| {}).unwrap();
    pic_process::decision::set(&root, &jpg, 1).unwrap();
    score::run_score_job(&root, &cfg, &opts, &|_| {}).unwrap();
    let csv = std::fs::read_to_string(&csv_path).unwrap();
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let headers = reader.headers().unwrap().clone();
    let row = reader.records().next().unwrap().unwrap();
    assert_eq!(row.get(headers.iter().position(|h| h == "stars").unwrap()), Some("1"));
    assert_eq!(row.get(headers.iter().position(|h| h == "rating_source").unwrap()), Some("manual"));
    assert_eq!(row.get(headers.iter().position(|h| h == "analysis_mode").unwrap()), Some("pixel"));
    let sidecar = std::fs::read_to_string(jpg.with_extension("xmp")).unwrap();
    assert!(sidecar.contains("<xmp:Rating>1</xmp:Rating>"));
    let snap = pic_process::review::snapshot::build_snapshot(
        &root, &cfg, &cache_path,
        &std::collections::HashMap::new()).unwrap();
    assert_eq!(snap.photos[0].stars, Some(1));
    assert_eq!(snap.photos[0].rating_source.as_deref(), Some("manual"));
    let _ = std::fs::remove_dir_all(root);
}

/// Option A 端到端验证：常规 `--xmp` 仅写 `firstcut:suggestedEV`；
/// `--xmp-exposure` 须显式启用，且绝不覆盖已有 Lightroom `crs:Exposure2012`。
#[test]
fn xmp_exposure_opt_in_and_manual_exposure_protection() {
    let root = std::env::temp_dir().join(format!(
        "firstcut_xmp_optin_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    // 暗图（码值 30）：必然落在曝光容差带外并产生正向建议 EV
    let dark_a = root.join("dark_a.jpg");
    let dark_b = root.join("dark_b.jpg");
    image::RgbImage::from_pixel(64, 64, image::Rgb([30, 30, 30])).save(&dark_a).unwrap();
    image::RgbImage::from_pixel(64, 64, image::Rgb([30, 30, 30])).save(&dark_b).unwrap();

    let csv_path = root.join("report.csv");
    let cache_path = root.join("cache.sqlite");
    let cfg = ScoreConfig::default();

    // 1) 未开启 --xmp 时，--xmp-exposure 必须拒绝
    let bad_opts = score::ScoreJobOptions {
        output_csv: csv_path.clone(), raw_csv: false, cache_path: cache_path.clone(), no_cache: false,
        no_ai: true, xmp: false, xmp_exposure: true, gpu: false, keep_override: None,
        include_raw: false, skip_processed: false,
    };
    assert!(score::run_score_job(&root, &cfg, &bad_opts, &|_| {}).is_err());

    // 2) 常规评分（--xmp，不带 --xmp-exposure）：仅写 firstcut:suggestedEV，不写 crs:Exposure2012
    let default_opts = score::ScoreJobOptions {
        xmp: true, xmp_exposure: false, ..bad_opts.clone()
    };
    score::run_score_job(&root, &cfg, &default_opts, &|_| {}).unwrap();
    let xmp_a = std::fs::read_to_string(dark_a.with_extension("xmp")).unwrap();
    assert!(xmp_a.contains("<firstcut:suggestedEV>+"), "常规评分应记录建议 EV 信息字段");
    assert!(!xmp_a.contains("crs:Exposure2012"), "常规评分默认不得写入 Adobe crs:Exposure2012");

    // 3) 模拟摄影师在 dark_b 的侧车里已手动调整了 Lightroom 曝光 +1.75 EV
    let xmp_b = std::fs::read_to_string(dark_b.with_extension("xmp")).unwrap();
    let xmp_b_manual = xmp_b.replace(
        "xmlns:firstcut=\"http://firstcut.local/ns/\">",
        "xmlns:firstcut=\"http://firstcut.local/ns/\"\n   xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"\n   crs:HasSettings=\"True\" crs:ProcessVersion=\"11.0\" crs:Exposure2012=\"+1.75\">",
    );
    assert!(xmp_b_manual.contains("crs:Exposure2012=\"+1.75\""));
    std::fs::write(dark_b.with_extension("xmp"), xmp_b_manual).unwrap();

    // 4) 显式开启 --xmp-exposure：dark_a 补写 crs:Exposure2012，dark_b 保留人工 +1.75
    let optin_opts = score::ScoreJobOptions {
        xmp: true, xmp_exposure: true, ..bad_opts
    };
    score::run_score_job(&root, &cfg, &optin_opts, &|_| {}).unwrap();
    let xmp_a2 = std::fs::read_to_string(dark_a.with_extension("xmp")).unwrap();
    let xmp_b2 = std::fs::read_to_string(dark_b.with_extension("xmp")).unwrap();
    assert!(xmp_a2.contains("crs:Exposure2012=\"+"), "显式开启应为无曝光参数的侧车写入建议 EV");
    assert!(xmp_a2.contains("crs:HasSettings=\"True\""));
    assert!(xmp_b2.contains("crs:Exposure2012=\"+1.75\""), "已有 Lightroom 人工曝光绝不得被覆盖");

    let _ = std::fs::remove_dir_all(root);
}

/// 构造带 EXIF DateTimeOriginal 的合法 JPEG 字节，便于端到端验证连拍分组 + 手动覆盖 + 导出。
fn save_jpeg_with_exif_datetime(path: &std::path::Path, rgb: [u8; 3], datetime: &str) {
    let mut jpg_bytes = Vec::new();
    let img = image::RgbImage::from_pixel(64, 64, image::Rgb(rgb));
    let mut cursor = std::io::Cursor::new(&mut jpg_bytes);
    img.write_to(&mut cursor, image::ImageFormat::Jpeg).unwrap();
    assert_eq!(&jpg_bytes[0..2], &[0xFF, 0xD8]);

    // 最小小端 TIFF EXIF：IFD0 包含 ExifIFDPointer (0x8769) -> ExifIFD 包含 DateTimeOriginal (0x9003)
    let dt_bytes = format!("{datetime}\0");
    assert_eq!(dt_bytes.len(), 20);
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II"); // little-endian
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset = 8
    // IFD0 at offset 8: 1 entry (12 bytes) + next IFD (4 bytes) => ends at 26
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x8769u16.to_le_bytes()); // ExifIFDPointer
    tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count = 1
    tiff.extend_from_slice(&26u32.to_le_bytes()); // ExifIFD offset = 26
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
    // ExifIFD at offset 26: 1 entry (12 bytes) + next IFD (4 bytes) => ends at 44
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x9003u16.to_le_bytes()); // DateTimeOriginal
    tiff.extend_from_slice(&2u16.to_le_bytes()); // ASCII
    tiff.extend_from_slice(&20u32.to_le_bytes()); // count = 20
    tiff.extend_from_slice(&44u32.to_le_bytes()); // value offset = 44
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
    tiff.extend_from_slice(dt_bytes.as_bytes());

    let mut app1 = Vec::new();
    app1.extend_from_slice(&[0xFF, 0xE1]);
    let payload_len = (2 + 6 + tiff.len()) as u16;
    app1.extend_from_slice(&payload_len.to_be_bytes());
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(&tiff);

    let mut out = Vec::with_capacity(jpg_bytes.len() + app1.len());
    out.extend_from_slice(&jpg_bytes[0..2]);
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpg_bytes[2..]);
    std::fs::write(path, out).unwrap();
}

/// 端到端验证：统一人工连拍覆盖（decisions.sqlite）贯穿 score CSV/XMP 与 CLI export，
/// 且导出子目录写入 `.firstcut-export` 后再次扫描相册不重复计数。
#[test]
fn unified_burst_override_and_cli_export_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("album");
    std::fs::create_dir_all(root.join("JPG")).unwrap();
    std::fs::create_dir_all(root.join("RAW")).unwrap();

    // 连拍组（间隔 1 秒）：b1、b2；非连拍单张（间隔 60 秒）：single
    let b1 = root.join("JPG").join("DSC00001.JPG");
    let b2 = root.join("JPG").join("DSC00002.JPG");
    let single = root.join("JPG").join("DSC00003.JPG");
    save_jpeg_with_exif_datetime(&b1, [120, 120, 120], "2026:09:27 12:00:00");
    save_jpeg_with_exif_datetime(&b2, [120, 120, 120], "2026:09:27 12:00:01");
    save_jpeg_with_exif_datetime(&single, [125, 125, 125], "2026:09:27 12:01:00");
    std::fs::write(root.join("RAW").join("DSC00001.ARW"), b"arw-1").unwrap();
    std::fs::write(root.join("RAW").join("DSC00002.ARW"), b"arw-2").unwrap();
    std::fs::write(root.join("RAW").join("DSC00003.ARW"), b"arw-3").unwrap();

    let csv_path = root.join(".firstcut").join("report.csv");
    let cache_path = root.join(".firstcut").join("cache.sqlite");
    let cfg = ScoreConfig::default();
    let opts = score::ScoreJobOptions {
        output_csv: csv_path.clone(), raw_csv: false, cache_path: cache_path.clone(), no_cache: false,
        no_ai: true, xmp: true, xmp_exposure: false, gpu: false, keep_override: Some(1),
        include_raw: true, skip_processed: true,
    };
    score::run_score_job(&root, &cfg, &opts, &|_| {}).unwrap();

    // 人工给三张都设 5★，并显式强制舍弃 DSC00001、强制保留 DSC00002
    pic_process::decision::set(&root, &b1, 5).unwrap();
    pic_process::decision::set(&root, &b2, 5).unwrap();
    pic_process::decision::set(&root, &single, 5).unwrap();
    pic_process::decision::set_burst_keep(&root, &b1, Some(false)).unwrap();
    pic_process::decision::set_burst_keep(&root, &b2, Some(true)).unwrap();

    // 重跑 score：CSV 与 XMP 中的 burst_keep 必须反映人工连拍决定
    score::run_score_job(&root, &cfg, &opts, &|_| {}).unwrap();
    let xmp1 = std::fs::read_to_string(root.join("RAW").join("DSC00001.xmp")).unwrap();
    let xmp2 = std::fs::read_to_string(root.join("RAW").join("DSC00002.xmp")).unwrap();
    assert!(xmp1.contains("<firstcut:burstKeep>false</firstcut:burstKeep>"));
    assert!(xmp2.contains("<firstcut:burstKeep>true</firstcut:burstKeep>"));

    // CLI export（--min-stars 4 --keep-only）：应导出非连拍单张 DSC00003 + 人工保留的 DSC00002，排除人工舍弃的 DSC00001
    let filter = pic_process::export::ExportFilter {
        min_stars: Some(4),
        keep_only: true,
        ..Default::default()
    };
    let (items, all_sources) =
        pic_process::export::collect_items_for_cli(&root, &cfg, &cache_path, &filter).unwrap();
    assert_eq!(items.len(), 2, "应命中 DSC00002(手动保留) + DSC00003(非连拍单张)");

    let exp_opts = pic_process::export::ExportOptions::default();
    let plan = pic_process::export::plan_export(&root, &items, &all_sources, &exp_opts).unwrap();
    let summary = pic_process::export::run_export(
        &plan,
        &std::sync::atomic::AtomicBool::new(false),
        &|_| {},
    )
    .unwrap();
    assert_eq!(summary.jpg_copied, 2);
    assert_eq!(summary.raw_copied, 2);
    assert_eq!(summary.xmp_written, 4);
    assert!(root.join("Selected/RAW/DSC00002.ARW").is_file());
    assert!(root.join("Selected/RAW/DSC00003.ARW").is_file());
    assert!(!root.join("Selected/RAW/DSC00001.ARW").exists());

    // 导出后再次扫描相册，条目数保持 6（3 JPG + 3 ARW），绝不把 Selected/ 重复扫入
    let rescanned = pic_process::scan::scan_directory(&root).unwrap();
    assert_eq!(rescanned.len(), 6);
    assert!(rescanned.iter().all(|e| e.has_pair));
}

/// 端到端验证双目录（双卡直连）+ 多品牌 RAW（NEF/CR3/RAF/RW2/ARW）+ 本机状态目录 + 源目录零写入 + 导出合并。
#[test]
fn dual_folder_multi_brand_raw_and_zero_source_write_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let jpg_card = temp.path().join("Card_JPG");
    let raw_card = temp.path().join("Card_RAW");
    let state_dir = temp.path().join("LocalState");
    let out_dir = temp.path().join("LocalExport");
    std::fs::create_dir_all(jpg_card.join("100MSDCF")).unwrap();
    std::fs::create_dir_all(raw_card.join("104MSDCF")).unwrap();

    let j1 = jpg_card.join("100MSDCF/IMG_0001.JPG");
    let j2 = jpg_card.join("100MSDCF/IMG_0002.JPG");
    let j3 = jpg_card.join("100MSDCF/IMG_0003.JPG");
    save_jpeg_with_exif_datetime(&j1, [120, 120, 120], "2026:11:25 10:00:00");
    save_jpeg_with_exif_datetime(&j2, [130, 130, 130], "2026:11:25 10:01:00");
    save_jpeg_with_exif_datetime(&j3, [140, 140, 140], "2026:11:25 10:02:00");
    std::fs::write(raw_card.join("104MSDCF/IMG_0001.NEF"), b"nikon-nef").unwrap();
    std::fs::write(raw_card.join("104MSDCF/IMG_0002.CR3"), b"canon-cr3").unwrap();
    std::fs::write(raw_card.join("104MSDCF/IMG_0003.RAF"), b"fuji-raf").unwrap();

    let source = pic_process::workspace::WorkspaceSource::dual(&jpg_card, &raw_card).unwrap();
    let paths = pic_process::workspace::WorkspacePaths::in_state_dir(state_dir.clone());
    paths.ensure_state_dir().unwrap();
    let cfg = ScoreConfig::default();
    let opts = score::ScoreJobOptions {
        output_csv: paths.report.clone(),
        raw_csv: false,
        cache_path: paths.cache.clone(),
        no_cache: false,
        no_ai: true,
        xmp: false,
        xmp_exposure: false,
        gpu: false,
        keep_override: None,
        include_raw: true,
        skip_processed: true,
    };
    score::run_score_job_in_workspace(&source, &paths, &cfg, &opts, &|_| {}).unwrap();
    pic_process::decision::set_in(&paths.state_dir, source.jpg_root(), &j1, 5).unwrap();
    pic_process::decision::set_in(&paths.state_dir, source.jpg_root(), &j2, 2).unwrap();
    pic_process::decision::set_in(&paths.state_dir, source.jpg_root(), &j3, 4).unwrap();

    // 源卡绝对零写入
    assert!(!jpg_card.join(".firstcut").exists());
    assert!(!raw_card.join(".firstcut").exists());
    assert!(!raw_card.join("104MSDCF/IMG_0001.xmp").exists());

    let filter = pic_process::export::ExportFilter {
        min_stars: Some(4),
        ..Default::default()
    };
    let (items, all_sources) = pic_process::export::collect_items_for_cli_in_workspace(
        &source,
        &paths,
        &cfg,
        &paths.cache,
        &filter,
    )
    .unwrap();
    assert_eq!(items.len(), 2);
    let exp_opts = pic_process::export::ExportOptions {
        target: pic_process::export::ExportTarget::ExternalDir(out_dir.clone()),
        ..Default::default()
    };
    let plan =
        pic_process::export::plan_export_workspace(&source, &items, &all_sources, &exp_opts)
            .unwrap();
    let summary = pic_process::export::run_export(
        &plan,
        &std::sync::atomic::AtomicBool::new(false),
        &|_| {},
    )
    .unwrap();
    assert_eq!(summary.jpg_copied, 2);
    assert_eq!(summary.raw_copied, 2);
    assert_eq!(summary.xmp_written, 4);
    assert!(out_dir.join("RAW/IMG_0001.NEF").is_file());
    assert!(out_dir.join("RAW/IMG_0003.RAF").is_file());
    assert!(!out_dir.join("RAW/IMG_0002.CR3").exists());
    assert!(std::fs::read_to_string(out_dir.join("RAW/IMG_0001.xmp"))
        .unwrap()
        .contains("<xmp:Rating>5</xmp:Rating>"));
}

#[test]
fn swapped_card_rescore_does_not_inherit_previous_manual_rating() {
    let temp = tempfile::tempdir().unwrap();
    let mount = temp.path().join("mount");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&mount).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    let photo = mount.join("DSC00001.JPG");
    save_jpeg_with_exif_datetime(&photo, [20, 40, 60], "2026:11:25 10:00:00");

    let source = pic_process::workspace::WorkspaceSource::single(&mount).unwrap();
    let paths = pic_process::workspace::WorkspacePaths::in_state_dir(state);
    paths.ensure_state_dir().unwrap();
    let cfg = ScoreConfig::default();
    let opts = score::ScoreJobOptions {
        output_csv: paths.report.clone(),
        raw_csv: false,
        cache_path: paths.cache.clone(),
        no_cache: false,
        no_ai: true,
        xmp: false,
        xmp_exposure: false,
        gpu: false,
        keep_override: None,
        include_raw: false,
        skip_processed: true,
    };

    score::run_score_job_in_workspace(&source, &paths, &cfg, &opts, &|_| {}).unwrap();
    pic_process::decision::set_in(&paths.state_dir, source.jpg_root(), &photo, 1).unwrap();

    // 换卡后同路径写入另一张完全不同的照片并重新评分
    save_jpeg_with_exif_datetime(&photo, [230, 220, 210], "2026:11:25 14:30:00");
    score::run_score_job_in_workspace(&source, &paths, &cfg, &opts, &|_| {}).unwrap();

    let report = std::fs::read_to_string(&paths.report).unwrap();
    assert!(
        report.contains(",\"5\",\"algorithm\","),
        "换卡重评分后应按新照片重新给星，不得继承旧卡的人工 1 星: {report}"
    );
    assert!(
        !report.contains(",\"manual\","),
        "换卡后不应出现 manual 覆盖记录: {report}"
    );
}
