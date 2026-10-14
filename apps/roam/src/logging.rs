//! 日志：同时输出到终端、日志文件（`<配置目录>/logs/roam.log`）和内存（界面「日志」页）。
//! 程序崩溃时把原因写进日志，方便事后排查。

use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// 内存里保留的最近日志行数。
const KEEP_LINES: usize = 1000;
/// 日志文件超过这个大小就轮换成 roam.log.1。
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

struct Sink {
    file: Option<File>,
    lines: VecDeque<String>,
    partial: Vec<u8>,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();
static DIR: OnceLock<PathBuf> = OnceLock::new();

fn sink() -> &'static Mutex<Sink> {
    SINK.get_or_init(|| {
        Mutex::new(Sink {
            file: None,
            lines: VecDeque::new(),
            partial: Vec::new(),
        })
    })
}

struct Writer;

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut sink = sink().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = &mut sink.file {
            let _ = file.write_all(buf);
        }
        sink.partial.extend_from_slice(buf);
        while let Some(pos) = sink.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = sink.partial.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line).trim_end().to_string();
            if !text.is_empty() {
                sink.lines.push_back(text);
                if sink.lines.len() > KEEP_LINES {
                    sink.lines.pop_front();
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = &mut sink().lock().unwrap_or_else(|e| e.into_inner()).file {
            file.flush()?;
        }
        Ok(())
    }
}

/// 初始化日志。`dir` 为日志目录。
pub fn init(dir: &Path) {
    init_named(dir, "roam.log");
}

/// 初始化日志，写到 `dir` 下的 `file`（系统服务和输入助手各用自己的文件）。
pub fn init_named(dir: &Path, file: &str) {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(file);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
        let _ = std::fs::rename(&path, dir.join(format!("{file}.1")));
    }
    sink().lock().unwrap().file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let _ = DIR.set(dir.to_path_buf());

    let filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,iroh=error,mdns_sd=error".into())
    };
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false).with_filter(filter()))
        .with(
            fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(|| Writer)
                .with_filter(filter()),
        )
        .init();

    // 崩溃时把原因和调用栈写进日志
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("程序崩溃: {info}\n{backtrace}");
        let _ = Writer.flush();
        default_hook(info);
    }));

    tracing::info!("Roam v{} 启动", roam_proto::VERSION);
}

/// 最近的日志行（最新的在最后）。
pub fn recent(limit: usize) -> Vec<String> {
    let sink = sink().lock().unwrap_or_else(|e| e.into_inner());
    let skip = sink.lines.len().saturating_sub(limit);
    sink.lines.iter().skip(skip).cloned().collect()
}

pub fn dir() -> Option<PathBuf> {
    DIR.get().cloned()
}

use tracing_subscriber::Layer as _;
