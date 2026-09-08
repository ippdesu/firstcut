//! 跑批任务状态（M-UI2）：单任务互斥 + 进度 + 日志环形缓冲
//!
//! 同一时刻只允许一个评分任务（全局单 job），状态经 `/api/job` 轮询。

use std::sync::Mutex;

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
    /// 最近日志（含时间不必要；截断到 200 行防万张跑批撑爆内存）
    log: Vec<String>,
}

#[derive(Debug)]
pub struct JobState {
    inner: Mutex<JobInner>,
}

const LOG_CAP: usize = 200;

impl JobState {
    pub fn new() -> Self {
        JobState {
            inner: Mutex::new(JobInner { status: JobStatus::Idle, log: Vec::new() }),
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
        if matches!(g.status, JobStatus::Running { .. }) {
            return false;
        }
        g.status = JobStatus::Running { done: 0, total: 0 };
        g.log.clear();
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
        g.log.push(line.into());
        if g.log.len() > LOG_CAP {
            let overflow = g.log.len() - LOG_CAP;
            g.log.drain(0..overflow);
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
        assert_eq!(job.status(), JobStatus::Failed { error: "坏了".into() });
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
        assert_eq!(log.last().unwrap(), &format!("l{}", LOG_CAP + 49));
        assert_eq!(log.first().unwrap(), &"l50".to_string(), "最老的被挤出");
    }
}
