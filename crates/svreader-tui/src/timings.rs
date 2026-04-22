//! Per-stage frame timings.
//!
//! Used both for the status-bar badge and for the timings log file.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

pub struct FrameTiming {
    pub render: Duration,
    pub compose: Duration,
    pub encode: Duration,
    pub write: Duration,
    pub other: Duration,
}

impl FrameTiming {
    pub fn total(&self) -> Duration {
        self.render + self.compose + self.encode + self.write + self.other
    }

    pub fn short_label(&self) -> String {
        fn ms(d: Duration) -> u64 {
            (d.as_secs_f64() * 1000.0).round() as u64
        }
        format!(
            "load:{}ms r:{} cm:{} en:{} tx:{} ot:{}",
            ms(self.total()),
            ms(self.render),
            ms(self.compose),
            ms(self.encode),
            ms(self.write),
            ms(self.other),
        )
    }
}

pub struct TimingsLog {
    file: Mutex<Option<File>>,
}

impl TimingsLog {
    pub fn open(path: Option<PathBuf>) -> Self {
        let file = match path {
            None => None,
            Some(p) => match File::create(&p) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!("failed to open timings log {:?}: {e}", p);
                    None
                }
            },
        };
        Self {
            file: Mutex::new(file),
        }
    }

    pub fn record(&self, page: usize, t: &FrameTiming) {
        let mut g = self.file.lock().unwrap();
        if let Some(f) = g.as_mut() {
            let _ = writeln!(
                f,
                "page={} {}  total={:?} render={:?} compose={:?} encode={:?} write={:?} other={:?}",
                page + 1,
                t.short_label(),
                t.total(),
                t.render,
                t.compose,
                t.encode,
                t.write,
                t.other
            );
            let _ = f.flush();
        }
    }
}
