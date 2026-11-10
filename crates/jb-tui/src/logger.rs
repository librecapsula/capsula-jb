//! The TUI log bridge: domain crates keep logging through the `log`
//! facade (the repo rule — no println), and here the records are
//! formatted CLI-style (`HH:MM:SS.mmm LEVEL  message`, UTC) and
//! shipped into the UI's log pane over a channel. After teardown the
//! sink is detached and records fall back to stderr, so a late
//! `log::error!` from the caller is not lost.

use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

static SINK: Mutex<Option<Sender<String>>> = Mutex::new(None);
static FILTER: OnceLock<Filter> = OnceLock::new();

/// Install the bridge globally (replaces whatever logger was there;
/// repeated installs only re-attach the sink — the global logger
/// itself can be set once per process).
pub fn install(tx: Sender<String>) {
    let _ = FILTER.set(Filter::from_env());
    *SINK.lock().unwrap() = Some(tx);
    let _ = log::set_boxed_logger(Box::new(TuiLog));
    log::set_max_level(log::LevelFilter::Trace);
}

/// Detach the channel: subsequent records go to stderr (the UI is gone
/// by then — a returned error, a panic path).
pub fn detach() {
    *SINK.lock().unwrap() = None;
}

struct TuiLog;

impl log::Log for TuiLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        FILTER.get().is_none_or(|f| f.enabled(metadata))
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("{} {:<5} {}", ts_ms(), record.level(), record.args());
        let guard = SINK.lock().unwrap();
        match guard.as_ref() {
            Some(tx) => {
                // the UI gone and the channel closed — fall through to
                // stderr rather than losing the record
                if tx.send(line.clone()).is_err() {
                    let _ = writeln!(std::io::stderr(), "{line}");
                }
            }
            None => {
                drop(guard);
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        }
    }

    fn flush(&self) {}
}

/// A minimal RUST_LOG filter: a default level plus `module=level`
/// overrides matched by the longest module-path prefix (the common
/// `RUST_LOG=info,jb_rockchip=debug` shape). Unparseable directives
/// fall back to the default level.
struct Filter {
    default: log::LevelFilter,
    modules: Vec<(String, log::LevelFilter)>,
}

impl Filter {
    fn from_env() -> Self {
        Self::parse(&std::env::var("RUST_LOG").unwrap_or_default())
    }

    fn parse(spec: &str) -> Self {
        let mut default = log::LevelFilter::Info;
        let mut modules = Vec::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match part.split_once('=') {
                None => {
                    if let Some(level) = parse_level(part) {
                        default = level;
                    }
                }
                Some((module, level)) => {
                    if let Some(level) = parse_level(level) {
                        modules.push((module.trim().to_string(), level));
                    }
                }
            }
        }
        Filter { default, modules }
    }

    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let target = metadata.target();
        let mut best: Option<(usize, log::LevelFilter)> = None;
        for (module, level) in &self.modules {
            if target == module || target.starts_with(&format!("{module}::")) {
                let len = module.len();
                if best.is_none_or(|(l, _)| len > l) {
                    best = Some((len, *level));
                }
            }
        }
        let allowed = best.map(|(_, l)| l).unwrap_or(self.default);
        metadata.level() <= allowed
    }
}

fn parse_level(s: &str) -> Option<log::LevelFilter> {
    s.trim().to_ascii_lowercase().parse().ok()
}

/// `HH:MM:SS.mmm` UTC of now (the CLI logger's format).
fn ts_ms() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_on(f: &Filter, target: &str, level: log::Level) -> bool {
        let rec = log::Record::builder()
            .level(level)
            .target(target)
            .args(format_args!(""))
            .build();
        f.enabled(rec.metadata())
    }

    #[test]
    fn filter_matches_global_and_module_levels() {
        let f = Filter::parse("info,jb_rockchip=debug");
        assert!(is_on(&f, "jb_rockchip", log::Level::Debug));
        assert!(is_on(&f, "jb_rockchip::session", log::Level::Debug));
        assert!(!is_on(&f, "jb_rockchip", log::Level::Trace));
        assert!(!is_on(&f, "jb_amlogic", log::Level::Debug));
        assert!(is_on(&f, "jb_amlogic", log::Level::Info));

        let f = Filter::parse("");
        assert_eq!(f.default, log::LevelFilter::Info);
        assert!(is_on(&f, "anything", log::Level::Warn));
        assert!(!is_on(&f, "anything", log::Level::Debug));
    }
}
