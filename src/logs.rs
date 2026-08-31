//! 日志收集:每服务一个 `LogHub`,提供历史回看 + 实时订阅 + 文件落盘三路。
//!
//! - 历史:`VecDeque` 环缓冲(默认 2000 行),供 `GET /logs?tail=N` 快照
//! - 实时:`broadcast` 通道,供 `GET /logs/stream` 的 SSE 推送
//! - 落盘:按日轮转 `data_dir/logs/<name>/YYYY-MM-DD.log`(可选)

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::Local;
use serde::Serialize;
use tokio::sync::broadcast;

/// 日志来源流。
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// 单行日志。
#[derive(Serialize, Clone, Debug)]
pub struct LogLine {
    pub stream: LogStream,
    /// 启发式识别的日志等级:debug/info/warn/error/unknown
    /// (被监护进程是黑盒,等级靠格式模式识别,见 supervisor::proc::detect_level)。
    pub level: String,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub text: String,
}

/// 常用等级名(warden 自身消息与告警用)。
pub const LEVEL_INFO: &str = "info";
pub const LEVEL_WARN: &str = "warn";
pub const LEVEL_ERROR: &str = "error";

const HISTORY_CAPACITY: usize = 2000;
const BROADCAST_CAPACITY: usize = 256;

/// 一个服务的日志收集器。
pub struct LogHub {
    history: Mutex<VecDeque<LogLine>>,
    tx: broadcast::Sender<LogLine>,
    file: Mutex<Option<RollingFile>>,
}

impl LogHub {
    pub fn new(file: Option<RollingFile>) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            history: Mutex::new(VecDeque::with_capacity(HISTORY_CAPACITY)),
            tx,
            file: Mutex::new(file),
        }
    }

    /// 推入一行:写历史 + 广播 + 落盘(若配置)。level 见 `LogLine::level`。
    pub fn push(&self, stream: LogStream, level: &str, text: impl Into<String>) {
        let line = LogLine {
            stream,
            level: level.to_string(),
            ts: chrono::Utc::now(),
            text: text.into(),
        };
        {
            let mut h = self.history.lock().unwrap();
            if h.len() >= HISTORY_CAPACITY {
                h.pop_front();
            }
            h.push_back(line.clone());
        }
        // 无订阅者时 send 返回 Err,属正常,忽略
        let _ = self.tx.send(line.clone());
        if let Some(rf) = self.file.lock().unwrap().as_mut() {
            if let Err(e) = rf.append_line(&line) {
                tracing::warn!("[logs] 落盘失败:{e}");
            }
        }
    }

    /// 最近 n 行(按时间升序)。
    pub fn snapshot(&self, n: usize) -> Vec<LogLine> {
        let h = self.history.lock().unwrap();
        let start = h.len().saturating_sub(n);
        h.iter().skip(start).cloned().collect()
    }

    /// 订阅实时流(之后产生的行)。
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }
}

/// 按日期轮转的日志文件(`dir/name/YYYY-MM-DD.log`)。
pub struct RollingFile {
    dir: PathBuf,
    name: String,
    current: Option<(String, File)>,
}

impl RollingFile {
    pub fn new(dir: PathBuf, name: String) -> Self {
        Self {
            dir,
            name,
            current: None,
        }
    }

    fn append_line(&mut self, line: &LogLine) -> io::Result<()> {
        let key = Local::now().format("%Y-%m-%d").to_string();
        let needs_reopen = match &self.current {
            None => true,
            Some((k, _)) => *k != key,
        };
        if needs_reopen {
            let dir = self.dir.join(&self.name);
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{key}.log"));
            let f = OpenOptions::new().create(true).append(true).open(&path)?;
            self.current = Some((key, f));
        }
        if let Some((_, f)) = &mut self.current {
            let ts = line.ts.format("%H:%M:%S%.3f");
            let tag = match line.stream {
                LogStream::Stdout => 'O',
                LogStream::Stderr => 'E',
            };
            writeln!(f, "{ts} {tag} {}", line.text)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> LogHub {
        LogHub::new(None)
    }

    #[test]
    fn push_and_snapshot_returns_last_n_in_order() {
        let h = hub();
        for i in 0..5 {
            h.push(LogStream::Stdout, "info", format!("line{i}"));
        }
        let snap = h.snapshot(3);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].text, "line2");
        assert_eq!(snap[2].text, "line4");
    }

    #[test]
    fn history_caps_at_capacity_dropping_oldest() {
        let h = hub();
        for i in 0..(HISTORY_CAPACITY + 100) {
            h.push(LogStream::Stdout, "info", format!("x{i}"));
        }
        let snap = h.snapshot(usize::MAX);
        assert_eq!(snap.len(), HISTORY_CAPACITY);
        // 最早的 100 行被弹出
        assert_eq!(snap[0].text, "x100");
    }

    #[test]
    fn snapshot_more_than_available_returns_all() {
        let h = hub();
        h.push(LogStream::Stderr, "error", "only");
        let snap = h.snapshot(500);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].stream, LogStream::Stderr);
    }

    #[test]
    fn broadcast_delivers_to_subscriber() {
        let h = hub();
        let mut rx = h.subscribe();
        h.push(LogStream::Stdout, "info", "hello");
        let line = rx.try_recv().expect("订阅者应收到广播");
        assert_eq!(line.text, "hello");
    }

    #[test]
    fn push_without_subscriber_does_not_panic() {
        let h = hub();
        h.push(LogStream::Stdout, "info", "nobody listening");
        // 仅断言未 panic
    }

    #[test]
    fn rolling_file_writes_under_dated_path() {
        let tmp = std::env::temp_dir().join(format!("warden-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut rf = RollingFile::new(tmp.clone(), "svc".into());
        rf.append_line(&LogLine {
            stream: LogStream::Stdout,
            level: "info".into(),
            ts: chrono::Utc::now(),
            text: "hello".into(),
        })
        .unwrap();
        let key = Local::now().format("%Y-%m-%d").to_string();
        let path = tmp.join("svc").join(format!("{key}.log"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("hello"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
