//! Diagnostics that always reach logcat, tag `AirFrameXR-diag`.
//!
//! The client's normal logger (`alvr_client_core::init_logging`) drops every record below the
//! streamer-set level once connected, so `info!` lines never show up while streaming. `diag!`
//! bypasses it: it writes straight to the Android log. It formats and allocates, so keep it out
//! of per-frame code (counters + a once-per-second summary only).

pub const DIAG_TAG: &str = "AirFrameXR-diag";

#[cfg(target_os = "android")]
pub fn write(message: &str) {
    use android_log_sys::{LogPriority, __android_log_write};
    use std::ffi::CString;

    // logcat truncates long lines; split on char boundaries so nothing is lost
    const MAX_LINE: usize = 3000;
    let tag = CString::new(DIAG_TAG).unwrap();

    let mut rest = message;
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_LINE);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (line, tail) = rest.split_at(end);
        if let Ok(text) = CString::new(line.replace('\0', "?")) {
            unsafe {
                __android_log_write(LogPriority::INFO as _, tag.as_ptr(), text.as_ptr());
            }
        }
        rest = tail;
    }
}

#[cfg(not(target_os = "android"))]
pub fn write(message: &str) {
    eprintln!("[{DIAG_TAG}] {message}");
}

#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {
        $crate::diag::write(&format!($($arg)*))
    };
}

// ---- Counters and series for the once-per-second `[diag]` summary --------------------------------
//
// Plain relaxed atomics so any thread (network, decoder, render) can feed them without a lock and
// without allocating; the render loop reads and resets them once per second.

use std::{
    sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed},
    time::Duration,
};

pub struct Counter(AtomicU32);

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    pub fn inc(&self) {
        self.0.fetch_add(1, Relaxed);
    }

    /// Returns the count since the last call and resets it.
    pub fn take(&self) -> u32 {
        self.0.swap(0, Relaxed)
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

/// Count, sum and maximum of durations.
pub struct Series {
    count: AtomicU32,
    sum_us: AtomicU64,
    max_us: AtomicU64,
}

#[derive(Clone, Copy, Default)]
pub struct SeriesSummary {
    pub count: u32,
    pub avg_ms: f32,
    pub max_ms: f32,
}

impl std::fmt::Display for SeriesSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1}/{:.1}", self.avg_ms, self.max_ms)
    }
}

impl Series {
    pub const fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    pub fn add(&self, duration: Duration) {
        let us = duration.as_micros() as u64;
        self.count.fetch_add(1, Relaxed);
        self.sum_us.fetch_add(us, Relaxed);
        self.max_us.fetch_max(us, Relaxed);
    }

    /// Returns the summary since the last call and resets the series.
    pub fn take(&self) -> SeriesSummary {
        let count = self.count.swap(0, Relaxed);
        let sum_us = self.sum_us.swap(0, Relaxed);
        let max_us = self.max_us.swap(0, Relaxed);
        SeriesSummary {
            count,
            avg_ms: if count > 0 {
                sum_us as f32 / count as f32 / 1000.0
            } else {
                0.0
            },
            max_ms: max_us as f32 / 1000.0,
        }
    }
}

impl Default for Series {
    fn default() -> Self {
        Self::new()
    }
}

// Network side (client_core receive loop)
pub static VIDEO_PACKETS: Counter = Counter::new();
pub static PACKET_LOSS: Counter = Counter::new(); // packets flagged with loss by the socket layer
pub static DROP_SATURATION: Counter = Counter::new(); // decoder refused the packet
pub static DROP_WAIT_IDR: Counter = Counter::new(); // dropped while waiting for a keyframe
pub static IDR_REQUESTS: Counter = Counter::new();
pub static RX_GAP_OVER: Counter = Counter::new(); // gaps between frames' packets > 1.5x nominal
pub static RX_GAP: Series = Series::new();

// Per-frame client stages (client_core statistics manager)
pub static DECODE: Series = Series::new(); // packet received -> decoded
pub static DECODER_QUEUE: Series = Series::new(); // decoded -> picked up by the renderer
pub static RENDERING: Series = Series::new(); // picked up -> submitted
pub static PIPELINE: Series = Series::new(); // pose sampled -> displayed (what the client reports)
pub static VSYNC_QUEUE: Series = Series::new();

// Staging pass (alvr_graphics)
pub static STAGING_IMPORTS: Counter = Counter::new();
pub static STAGING_FAILURES: Counter = Counter::new();
