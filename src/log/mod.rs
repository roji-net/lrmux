// Logging: file + optional remote syslog (UDP RFC 3164).
// Minimal, no external dependencies.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn syslog_priority(self) -> u8 {
        match self {
            Level::Debug => 7, // LOG_DEBUG
            Level::Info => 6,  // LOG_INFO
            Level::Warn => 4,  // LOG_WARNING
            Level::Error => 3, // LOG_ERR
        }
    }
}

struct Logger {
    file: Option<Mutex<std::fs::File>>,
    syslog: Option<SyslogConfig>,
    min_level: Level,
}

struct SyslogConfig {
    socket: UdpSocket,
    host: String,
    port: u16,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Initialize the global logger.
///
/// - `log_dir`: directory for log files (e.g. `/tmp/lrmux-<UID>/logs`).
/// - `server_name`: server name, used in the log filename.
/// - `min_level`: minimum level to log.
/// - `syslog`: optional (host, port) for remote syslog over UDP.
pub fn init(log_dir: &str, server_name: &str, min_level: Level, syslog: Option<(String, u16)>) {
    // Create log directory.
    let _ = std::fs::create_dir_all(log_dir);

    let log_path: PathBuf = PathBuf::from(log_dir).join(format!("{server_name}.log"));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok()
        .map(Mutex::new);

    let syslog = syslog.and_then(|(host, port)| {
        UdpSocket::bind("0.0.0.0:0")
            .ok()
            .map(|socket| SyslogConfig { socket, host, port })
    });

    let _ = LOGGER.set(Logger {
        file,
        syslog,
        min_level,
    });
}

/// Install a panic hook that logs the panic before the process dies.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".to_string());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "panic".to_string()
        };
        log_raw(Level::Error, &format!("PANIC at {location}: {msg}"));
        // Also print to stderr as usual.
        default_hook(info);
    }));
}

/// Log a message at the given level.
pub fn log(level: Level, msg: &str) {
    log_raw(level, msg);
}

pub fn debug(msg: &str) {
    log_raw(Level::Debug, msg);
}

pub fn info(msg: &str) {
    log_raw(Level::Info, msg);
}

pub fn warn(msg: &str) {
    log_raw(Level::Warn, msg);
}

pub fn error(msg: &str) {
    log_raw(Level::Error, msg);
}

fn log_raw(level: Level, msg: &str) {
    if let Some(logger) = LOGGER.get() {
        if level < logger.min_level {
            return;
        }
        let timestamp = format_timestamp();
        let line = format!("[{timestamp}] [{level}] {msg}\n", level = level.as_str());

        // Write to file.
        if let Some(ref file) = logger.file
            && let Ok(mut f) = file.lock()
        {
            let _ = f.write_all(line.as_bytes());
        }

        // Send to remote syslog (UDP RFC 3164).
        if let Some(ref syslog) = logger.syslog {
            let hostname = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("USER"))
                .unwrap_or_else(|_| "lrmux".to_string());
            let packet = format!(
                "<{pri}>{timestamp} {hostname} lrmux: {msg}\n",
                pri = level.syslog_priority(),
                timestamp = format_syslog_timestamp(),
            );
            let addr = format!("{}:{}", syslog.host, syslog.port);
            let _ = syslog.socket.send_to(packet.as_bytes(), &addr);
        }
    }
}

fn format_timestamp() -> String {
    let now = SystemTime::now();
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let (mins, secs) = (rem / 60, rem % 60);
    // Simple date calculation from days since epoch (1970-01-01).
    let (year, month, day) = days_to_date(days as i64);
    format!("{year:04}-{month:02}-{day:02} {hours:02}:{mins:02}:{secs:02}")
}

fn format_syslog_timestamp() -> String {
    let now = SystemTime::now();
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let (mins, secs) = (rem / 60, rem % 60);
    let (_year, month, day) = days_to_date(days as i64);
    let month_name = match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    };
    format!("{month_name} {day:2} {hours:02}:{mins:02}:{secs:02}")
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
fn days_to_date(days: i64) -> (i64, i64, i64) {
    // Algorithm from Howard Hinnant's date library (civil_from_days).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
