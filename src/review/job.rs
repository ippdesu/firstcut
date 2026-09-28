//! 跑批任务状态：单任务互斥 + 进度 + 有界内存与磁盘日志
//!
//! 同一时刻只允许一个评分任务（全局单 job），状态经 `/api/job` 轮询。

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

/// 任务状态机：Idle → Running → Done/Failed
#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Idle,
    Running { done: usize, total: usize },
    Done { summary: String },
    Failed { error: String },
}

#[derive(Debug)]
struct JobInner {
    status: JobStatus,
    stopping: bool,
    /// 最近 200 行日志，供 UI 即时显示。
    log: Vec<String>,
    /// 磁盘写入连续失败后停止重试，避免刷屏和无意义磁盘访问。
    log_file_disabled: bool,
}

#[derive(Debug)]
pub struct JobState {
    inner: Mutex<JobInner>,
    log_file: Option<PathBuf>,
}

const LOG_CAP: usize = 200;
const LOG_LINE_CAP_BYTES: usize = 8 * 1024;
const LOG_FILE_CAP_BYTES: u64 = 256 * 1024;
const LOG_COMPACT_TARGET_BYTES: u64 = LOG_FILE_CAP_BYTES / 2;

impl JobState {
    pub fn new() -> Self {
        Self::with_optional_log_file(None)
    }

    /// 创建将任务记录追加到指定文件的状态机。文件仅在首次记录时创建。
    pub fn with_log_file(path: PathBuf) -> Self {
        Self::with_optional_log_file(Some(path))
    }

    fn with_optional_log_file(log_file: Option<PathBuf>) -> Self {
        JobState {
            inner: Mutex::new(JobInner {
                status: JobStatus::Idle,
                stopping: false,
                log: Vec::new(),
                log_file_disabled: false,
            }),
            log_file,
        }
    }

    pub fn status(&self) -> JobStatus {
        self.inner.lock().unwrap().status.clone()
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status(), JobStatus::Running { .. })
    }

    /// 尝试占坑：Idle/Done/Failed 时进入 Running 并清空日志；已在跑则返回 false
    pub fn begin(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.stopping || matches!(g.status, JobStatus::Running { .. }) {
            return false;
        }
        g.status = JobStatus::Running { done: 0, total: 0 };
        g.log.clear();
        g.log_file_disabled = false;
        true
    }

    /// 只有评分空闲时才能关闭服务；与 begin 共用锁，避免关闭与新跑批竞态。
    pub fn begin_shutdown(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.stopping || matches!(g.status, JobStatus::Running { .. }) {
            return false;
        }
        g.stopping = true;
        true
    }

    pub fn progress(&self, done: usize, total: usize) {
        let mut g = self.inner.lock().unwrap();
        if let JobStatus::Running { done: d, total: t } = &mut g.status {
            *d = done;
            *t = total;
        }
    }

    pub fn log(&self, line: impl Into<String>) {
        let mut g = self.inner.lock().unwrap();
        let entry = bounded_line(format!("{} {}", local_timestamp(), line.into()));
        g.log.push(entry.clone());
        if g.log.len() > LOG_CAP {
            let overflow = g.log.len() - LOG_CAP;
            g.log.drain(0..overflow);
        }
        if !g.log_file_disabled {
            if let Some(path) = &self.log_file {
                if let Err(err) = append_bounded(path, &entry) {
                    g.log_file_disabled = true;
                    g.log.push(format!(
                        "{} [log] 磁盘日志写入失败，已停止本次写入: {err}",
                        local_timestamp()
                    ));
                    if g.log.len() > LOG_CAP {
                        let overflow = g.log.len() - LOG_CAP;
                        g.log.drain(0..overflow);
                    }
                }
            }
        }
    }

    pub fn finish(&self, summary: String) {
        self.inner.lock().unwrap().status = JobStatus::Done { summary };
    }

    pub fn fail(&self, error: String) {
        self.inner.lock().unwrap().status = JobStatus::Failed { error };
    }

    /// 快照（供 /api/job 序列化）
    pub fn snapshot(&self) -> (JobStatus, Vec<String>) {
        let g = self.inner.lock().unwrap();
        (g.status.clone(), g.log.clone())
    }
}

fn bounded_line(mut line: String) -> String {
    if line.len() <= LOG_LINE_CAP_BYTES {
        return line;
    }
    let suffix = " …[该行过长，已截断]";
    let mut end = LOG_LINE_CAP_BYTES.saturating_sub(suffix.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line.truncate(end);
    line.push_str(suffix);
    line
}

/// 追加一行并将单个日志文件限制在 256 KiB。超过上限时先保留末尾约 128 KiB。
fn append_bounded(path: &PathBuf, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    let line = format!("{line}\n");
    let file_len = file.metadata()?.len();
    if file_len.saturating_add(line.len() as u64) > LOG_FILE_CAP_BYTES {
        let keep = LOG_COMPACT_TARGET_BYTES.min(file_len);
        file.seek(SeekFrom::Start(file_len.saturating_sub(keep)))?;
        let mut tail = Vec::with_capacity(keep as usize);
        Read::by_ref(&mut file).take(keep).read_to_end(&mut tail)?;
        if file_len > keep {
            if let Some(newline) = tail.iter().position(|byte| *byte == b'\n') {
                tail.drain(..=newline);
            } else {
                tail.clear();
            }
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&tail)?;
    }
    file.seek(SeekFrom::End(0))?;
    file.write_all(line.as_bytes())?;
    file.flush()
}

#[cfg(windows)]
fn local_timestamp() -> String {
    #[repr(C)]
    #[derive(Default)]
    struct LocalSystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[link(name = "Kernel32")]
    extern "system" {
        fn GetLocalTime(system_time: *mut LocalSystemTime);
    }
    let mut now = LocalSystemTime::default();
    // GetLocalTime fills the fixed-layout SYSTEMTIME structure.
    unsafe { GetLocalTime(&mut now) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    )
}

#[cfg(not(windows))]
fn local_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    format!(
        "UTC {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year,
        month,
        day,
        day_seconds / 3_600,
        (day_seconds % 3_600) / 60,
        day_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_job_mutex() {
        let job = JobState::new();
        assert!(job.begin(), "空闲可启动");
        assert!(!job.begin(), "运行中不可重复启动");
        job.progress(5, 10);
        job.log("一行日志");
        match job.status() {
            JobStatus::Running { done, total } => {
                assert_eq!((done, total), (5, 10))
            }
            other => panic!("应为 Running: {other:?}"),
        }
        job.finish("完成".into());
        assert!(job.begin(), "完成后可再次启动");
        assert!(job.snapshot().1.is_empty(), "重启清空日志");
        job.fail("坏了".into());
        assert_eq!(
            job.status(),
            JobStatus::Failed {
                error: "坏了".into()
            }
        );
    }

    #[test]
    fn log_ring_is_capped() {
        let job = JobState::new();
        job.begin();
        for i in 0..(LOG_CAP + 50) {
            job.log(format!("l{i}"));
        }
        let (_, log) = job.snapshot();
        assert_eq!(log.len(), LOG_CAP);
        assert!(log.last().unwrap().ends_with(&format!("l{}", LOG_CAP + 49)));
        assert!(log.first().unwrap().ends_with("l50"), "最老的被挤出");
    }

    #[test]
    fn shutdown_waits_for_scoring_and_blocks_new_jobs() {
        let job = JobState::new();
        assert!(job.begin());
        assert!(!job.begin_shutdown());
        job.finish("完成".into());
        assert!(job.begin_shutdown());
        assert!(!job.begin());
        assert!(!job.begin_shutdown());
    }
}
