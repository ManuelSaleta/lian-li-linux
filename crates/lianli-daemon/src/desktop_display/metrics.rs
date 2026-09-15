use std::time::{Duration, Instant};

pub(super) struct StreamMetrics {
    since: Instant,
    frames: u64,
    bytes: u64,
    largest_packet: usize,
    request: Duration,
    request_max: Duration,
    usb: Duration,
    usb_max: Duration,
}

impl StreamMetrics {
    pub fn new() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            bytes: 0,
            largest_packet: 0,
            request: Duration::ZERO,
            request_max: Duration::ZERO,
            usb: Duration::ZERO,
            usb_max: Duration::ZERO,
        }
    }

    pub fn delivered(&mut self, pid: u16, bytes: usize, request: Duration, usb: Duration) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        self.frames += 1;
        self.bytes += bytes as u64;
        self.largest_packet = self.largest_packet.max(bytes);
        self.request += request;
        self.request_max = self.request_max.max(request);
        self.usb += usb;
        self.usb_max = self.usb_max.max(usb);
        let elapsed = self.since.elapsed();
        if elapsed < Duration::from_secs(5) {
            return;
        }
        tracing::debug!(
            pid = format_args!("{pid:04x}"),
            fps = self.frames as f64 / elapsed.as_secs_f64(),
            megabits_per_second = self.bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0,
            largest_packet_bytes = self.largest_packet,
            request_average_ms = self.request.as_secs_f64() * 1000.0 / self.frames as f64,
            request_max_ms = self.request_max.as_secs_f64() * 1000.0,
            usb_average_ms = self.usb.as_secs_f64() * 1000.0 / self.frames as f64,
            usb_max_ms = self.usb_max.as_secs_f64() * 1000.0,
            "Desktop delivery timing (USB completion does not acknowledge panel presentation)"
        );
        *self = Self::new();
    }
}
