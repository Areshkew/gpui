use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use util::ResultExt;
use windows::Win32::{
    Foundation::HWND,
    Graphics::Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo},
    System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
};

static QPC_TICKS_PER_SECOND: LazyLock<u64> = LazyLock::new(|| {
    let mut frequency = 0;
    // On systems that run Windows XP or later, the function will always succeed and
    // will thus never return zero.
    unsafe { QueryPerformanceFrequency(&mut frequency).unwrap() };
    frequency as u64
});

const VSYNC_INTERVAL_THRESHOLD: Duration = Duration::from_millis(1);
const DEFAULT_VSYNC_INTERVAL: Duration = Duration::from_micros(16_666); // ~60Hz

pub(crate) struct VSyncProvider {
    interval: Duration,
    f: Box<dyn Fn() -> bool>,
}

impl VSyncProvider {
    pub(crate) fn new() -> Self {
        let interval = get_dwm_interval()
            .context("Failed to get DWM interval")
            .log_err()
            .unwrap_or(DEFAULT_VSYNC_INTERVAL);
        let f = Box::new(|| unsafe { DwmFlush().is_ok() });
        Self { interval, f }
    }

    pub(crate) fn wait_for_vsync(&self) {
        let vsync_start = Instant::now();
        let wait_succeeded = (self.f)();
        let elapsed = vsync_start.elapsed();
        // DwmFlush and DCompositionWaitForCompositorClock returns very early
        // instead of waiting until vblank when the monitor goes to sleep or is
        // unplugged (nothing to present due to desktop occlusion). We use 1ms as
        // a threshold for the duration of the wait functions and fallback to
        // Sleep() if it returns before that. This could happen during normal
        // operation for the first call after the vsync thread becomes non-idle,
        // but it shouldn't happen often.
        if !wait_succeeded || elapsed < VSYNC_INTERVAL_THRESHOLD {
            log::trace!("VSyncProvider::wait_for_vsync() took less time than expected");
            std::thread::sleep(self.interval);
        } else {
            wait_until_before_vblank(DRAW_LEAD);
        }
    }
}

fn get_dwm_interval() -> Result<Duration> {
    let mut timing_info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing_info) }?;
    let interval = retrieve_duration(timing_info.qpcRefreshPeriod, *QPC_TICKS_PER_SECOND);
    // Check for interval values that are impossibly low. A 29 microsecond
    // interval was seen (from a qpcRefreshPeriod of 60).
    if interval < VSYNC_INTERVAL_THRESHOLD {
        Ok(retrieve_duration(
            timing_info.rateRefresh.uiDenominator as u64,
            timing_info.rateRefresh.uiNumerator as u64,
        ))
    } else {
        Ok(interval)
    }
}

#[inline]
fn retrieve_duration(counts: u64, ticks_per_second: u64) -> Duration {
    let ticks_per_microsecond = ticks_per_second / 1_000_000;
    Duration::from_micros(counts / ticks_per_microsecond)
}

/// How long before the next vblank windows draw.
///
/// DwmFlush returns just after DWM composed a frame, which is too late for
/// anything presented then: it waits for the composition after, and reaches
/// the screen almost two refreshes later. Drawing this long before the vblank
/// makes the next composition instead, a refresh earlier. A shorter lead
/// misses it whenever drawing takes a few milliseconds. On displays whose
/// refresh is shorter than this there is no time left to wait, and nothing
/// changes.
const DRAW_LEAD: Duration = Duration::from_millis(8);

/// Sleeps until `lead` before the next vblank.
fn wait_until_before_vblank(lead: Duration) {
    let mut timing_info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    if unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing_info) }.is_err() {
        return;
    }
    let period = timing_info.qpcRefreshPeriod as i64;
    let vblank = timing_info.qpcVBlank as i64;
    let mut now = 0;
    if period <= 0 || unsafe { QueryPerformanceCounter(&mut now) }.is_err() {
        return;
    }
    // The first vblank after now.
    let next = if vblank > now {
        vblank - (vblank - now) / period * period
    } else {
        vblank + ((now - vblank) / period + 1) * period
    };
    let ticks = *QPC_TICKS_PER_SECOND as i64;
    let target = next - (lead.as_micros() as i64) * ticks / 1_000_000;
    if target > now {
        std::thread::sleep(Duration::from_micros(
            ((target - now) * 1_000_000 / ticks) as u64,
        ));
    }
}
