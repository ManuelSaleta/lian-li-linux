use super::acquire::{AcquiredCursor, AcquiredFrame};
use super::{Node, OwnedOutput};
use crate::frame::{Frame, FrameDamage, Mode, PixelFormat};
use crate::gpu::{Converter, CursorLayer, GpuFrame, Layer};
use crate::{Capture, Event, OutputRequest};
use anyhow::{ensure, Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct HermesCapture {
    output: OwnedOutput,
    converter: Converter,
    request: OutputRequest,
    primary: Option<AcquiredFrame>,
    damage: FrameDamage,
    pending_damage: FrameDamage,
    cursor: Option<AcquiredCursor>,
    base_pixels: Vec<u8>,
    cursor_pixels: Vec<u8>,
    pixels: Vec<u8>,
    announced: bool,
    pending: bool,
    refresh: bool,
    powered: bool,
    status_at: Instant,
    observed: (u64, u64),
    dirty: (bool, bool),
    retry_at: Instant,
    composite_dirty: bool,
    snapshot_retries: u8,
    cpu_primary_dirty: bool,
    cpu_cursor_dirty: bool,
    cpu_frame_dirty: bool,
}

impl HermesCapture {
    fn timestamp(&self) -> Option<crate::frame::CaptureTimestamp> {
        let primary = self.primary.as_ref()?;
        let nanos = primary
            .timestamp_ns
            .max(self.cursor.as_ref().map_or(0, |cursor| cursor.timestamp_ns));
        Some(crate::frame::CaptureTimestamp {
            clock: crate::frame::CaptureClock::Monotonic,
            since_epoch: Duration::from_nanos(nanos),
        })
    }

    pub fn open(request: OutputRequest, cancel: &AtomicBool) -> Result<Self> {
        request.validate()?;
        let paths = Node::candidates().context("No Hermes-KMS render nodes are visible")?;
        let output = claim_output(&paths, &request, cancel)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        let primary = loop {
            ensure!(!cancel.load(Ordering::Relaxed), "Hermes startup cancelled");
            ensure!(
                Instant::now() < deadline,
                "Session compositor did not activate the owned Hermes output"
            );
            let status = output.status()?;
            if status.flags & ((1 << 2) | (1 << 3)) == (1 << 2) | (1 << 3) {
                let active = Mode {
                    width: status.active_width,
                    height: status.active_height,
                    refresh_hz: status.active_refresh_hz,
                };
                request.validate_mode(active)?;
                ensure!(
                    active == request.preferred,
                    "Compositor selected another Hermes mode"
                );
                if let Some(frame) = output.acquire_frame()? {
                    ensure!(
                        (frame.image.width, frame.image.height) == (active.width, active.height),
                        "Hermes scanout buffer does not match the panel mode"
                    );
                    break frame;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let mut errors = Vec::new();
        let mut selected = None;
        let mut base_pixels = Vec::new();
        let gpu_deadline = Instant::now() + Duration::from_secs(4);
        for path in paths.iter().take(8) {
            ensure!(
                !cancel.load(Ordering::Relaxed),
                "GPU capture setup cancelled"
            );
            if selected.is_some() {
                break;
            }
            ensure!(Instant::now() < gpu_deadline, "GPU capture setup timed out");
            let result: Result<Converter> = (|| {
                let mut converter = Converter::open(path)?;
                converter.readback(&primary.image, Layer::Desktop, &mut base_pixels, cancel)?;
                Ok(converter)
            })();
            match result {
                Ok(converter) => selected = Some(converter),
                Err(error) => {
                    if errors.len() < 4 {
                        errors.push(format!("{}: {error:#}", path.display()));
                    }
                }
            }
        }
        let converter = selected.with_context(|| {
            format!("No GPU can read back Hermes scanout: {}", errors.join("\n"))
        })?;
        let mut capture = Self {
            output,
            converter,
            request,
            primary: Some(primary),
            damage: FrameDamage::Full,
            pending_damage: FrameDamage::Unchanged,
            cursor: None,
            pixels: base_pixels.clone(),
            base_pixels,
            cursor_pixels: Vec::new(),
            announced: false,
            pending: true,
            refresh: false,
            powered: true,
            status_at: Instant::now(),
            observed: (0, 0),
            dirty: (false, false),
            retry_at: Instant::now(),
            composite_dirty: false,
            snapshot_retries: 0,
            cpu_primary_dirty: false,
            cpu_cursor_dirty: true,
            cpu_frame_dirty: true,
        };
        ensure!(
            capture.update_cursor()?,
            "Cursor changed during Hermes startup. Retry capture."
        );
        capture.observed = (
            capture.primary.as_ref().unwrap().sequence,
            capture.cursor.as_ref().map_or(0, |cursor| cursor.sequence),
        );
        capture.prepare_cpu(cancel)?;
        Ok(capture)
    }

    fn update_cursor(&mut self) -> Result<bool> {
        let Some(cursor) = self.output.acquire_cursor()? else {
            return Ok(false);
        };
        if cursor.image.is_some() {
            if self
                .cursor
                .as_ref()
                .is_none_or(|previous| previous.image_sequence != cursor.image_sequence)
                || self.cursor_pixels.is_empty()
            {
                self.cpu_cursor_dirty = true;
            }
        } else {
            self.cursor_pixels.clear();
            self.cpu_cursor_dirty = false;
        }
        self.cursor = Some(cursor);
        Ok(true)
    }

    fn prepare_cpu(&mut self, cancel: &AtomicBool) -> Result<()> {
        if self.cpu_primary_dirty {
            self.converter.readback(
                &self.primary.as_ref().context("Hermes has no frame")?.image,
                Layer::Desktop,
                &mut self.base_pixels,
                cancel,
            )?;
            self.cpu_primary_dirty = false;
        }
        if self.cpu_cursor_dirty {
            if let Some(image) = self
                .cursor
                .as_ref()
                .and_then(|cursor| cursor.image.as_ref())
            {
                self.converter
                    .readback(image, Layer::Cursor, &mut self.cursor_pixels, cancel)?;
            }
            self.cpu_cursor_dirty = false;
        }
        if self.cpu_frame_dirty {
            self.compose()?;
            self.cpu_frame_dirty = false;
        }
        Ok(())
    }

    fn compose(&mut self) -> Result<()> {
        self.pixels.clone_from(&self.base_pixels);
        if let Some(cursor) = self.cursor.as_ref().filter(|cursor| cursor.visible) {
            overlay_cursor(
                &mut self.pixels,
                self.request.preferred,
                cursor,
                &self.cursor_pixels,
            )?;
        }
        Ok(())
    }

    fn retry_snapshot(&mut self) -> Result<Vec<Event>> {
        self.snapshot_retries = self.snapshot_retries.saturating_add(1);
        ensure!(
            self.snapshot_retries <= 8,
            "Hermes could not acquire a stable frame/cursor snapshot after eight attempts"
        );
        self.retry_at = Instant::now()
            + Duration::from_millis(10 << self.snapshot_retries.saturating_sub(1).min(3));
        Ok(Vec::new())
    }
}

impl Capture for HermesCapture {
    fn poll_events(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<Vec<Event>> {
        ensure!(!cancel.load(Ordering::Relaxed), "Hermes capture cancelled");
        if !self.announced && !self.refresh {
            self.announced = true;
            self.pending = false;
            return Ok(vec![
                Event::ModeChanged(self.request.preferred, PixelFormat::Abgr8888),
                Event::FrameReady,
            ]);
        }
        if Instant::now() < self.retry_at {
            std::thread::sleep(
                self.retry_at
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(10)),
            );
            return Ok(Vec::new());
        }
        if !(self.pending && self.powered && (self.refresh || self.dirty.0 || self.dirty.1)) {
            if let Some(update) =
                self.output
                    .wait_update(self.observed.0, self.observed.1, timeout, cancel)?
            {
                self.observed = (update.frame_sequence, update.cursor_sequence);
                self.dirty.0 |= update.flags & 1 != 0;
                self.dirty.1 |= update.flags & 2 != 0;
            }
        }
        if self.status_at.elapsed() >= Duration::from_secs(1) {
            self.status_at = Instant::now();
            let status = self.output.status()?;
            let powered = status.flags & (1 << 2) != 0;
            if powered != self.powered {
                self.powered = powered;
                return Ok(vec![Event::PowerChanged(powered)]);
            }
            if powered {
                ensure!(
                    (
                        status.active_width,
                        status.active_height,
                        status.active_refresh_hz
                    ) == (
                        self.request.preferred.width,
                        self.request.preferred.height,
                        self.request.preferred.refresh_hz
                    ),
                    "Hermes compositor mode changed during playback"
                );
            }
        }
        if !self.pending || !self.powered {
            return Ok(Vec::new());
        }
        let frame_changed = self.refresh || self.dirty.0;
        let cursor_changed = self.refresh || self.dirty.1;
        if frame_changed {
            let Some(frame) = self.output.acquire_frame()? else {
                if self.output.status()?.flags & (1 << 2) == 0 {
                    self.powered = false;
                    return Ok(vec![Event::PowerChanged(false)]);
                }
                return self.retry_snapshot();
            };
            ensure!(
                (frame.image.width, frame.image.height)
                    == (self.request.preferred.width, self.request.preferred.height),
                "Hermes frame dimensions changed during capture"
            );
            let layout_changed = self.primary.as_ref().is_none_or(|previous| {
                previous.image.fourcc != frame.image.fourcc
                    || previous.image.modifier != frame.image.modifier
            });
            let unchanged = !self.refresh
                && !layout_changed
                && frame
                    .damage
                    .is_some_and(|[x1, y1, x2, y2]| x1 == x2 || y1 == y2);
            if !unchanged {
                self.cpu_primary_dirty = true;
                self.composite_dirty = true;
            }
            let damage = if self.refresh || layout_changed {
                FrameDamage::Full
            } else if let Some(bounds) = frame.damage {
                FrameDamage::rectangle(self.request.preferred, bounds)?
            } else {
                FrameDamage::Unknown
            };
            self.pending_damage = self.pending_damage.union(damage);
            self.observed.0 = self.observed.0.max(frame.sequence);
            self.primary = Some(frame);
        }
        if cursor_changed {
            self.pending_damage = FrameDamage::Full;
            if !self.update_cursor()? {
                return self.retry_snapshot();
            }
        }
        self.composite_dirty |= cursor_changed;
        if frame_changed || cursor_changed {
            self.snapshot_retries = 0;
            if let Some(cursor) = &self.cursor {
                self.observed.1 = self.observed.1.max(cursor.sequence);
            }
            self.refresh = false;
            self.dirty = (false, false);
            if !self.composite_dirty {
                return Ok(Vec::new());
            }
            self.cpu_frame_dirty = true;
            self.composite_dirty = false;
            self.pending = false;
            self.damage = std::mem::replace(&mut self.pending_damage, FrameDamage::Unchanged);
            let mut events = Vec::with_capacity(2);
            if !self.announced {
                self.announced = true;
                events.push(Event::ModeChanged(
                    self.request.preferred,
                    PixelFormat::Abgr8888,
                ));
            }
            events.push(Event::FrameReady);
            return Ok(events);
        }
        Ok(Vec::new())
    }

    fn request_update(&mut self) -> Result<bool> {
        self.pending = true;
        Ok(false)
    }

    fn frame(&mut self, cancel: &AtomicBool) -> Result<Frame<'_>> {
        ensure!(
            !self.refresh && self.powered,
            "A fresh active Hermes frame is required"
        );
        self.prepare_cpu(cancel)?;
        Frame::new(
            self.request.preferred,
            PixelFormat::Abgr8888,
            self.request.preferred.width as usize * 4,
            &self.pixels,
        )
        .and_then(|frame| frame.with_damage(self.damage))
        .map(|frame| frame.with_timestamp(self.timestamp()))
    }

    fn gpu_frame(&mut self, cancel: &AtomicBool) -> Result<Option<GpuFrame<'_>>> {
        let timestamp = self.timestamp();
        let damage = self.damage;
        ensure!(
            !self.refresh && self.powered,
            "A fresh active Hermes frame is required"
        );
        self.base_pixels = Vec::new();
        self.cursor_pixels = Vec::new();
        self.pixels = Vec::new();
        self.cpu_primary_dirty = true;
        self.cpu_cursor_dirty = true;
        self.cpu_frame_dirty = true;
        let cursor = self
            .cursor
            .as_ref()
            .filter(|cursor| cursor.visible)
            .map(|cursor| -> Result<CursorLayer<'_>> {
                Ok(CursorLayer {
                    image: cursor
                        .image
                        .as_ref()
                        .context("Visible cursor has no image")?,
                    destination: cursor.destination,
                    source: cursor.source,
                })
            })
            .transpose()?;
        self.converter
            .compose_export(
                &self.primary.as_ref().context("Hermes has no frame")?.image,
                cursor,
                cancel,
            )
            .map(|mut frame| {
                frame.timestamp = timestamp;
                frame.damage = damage;
                Some(frame)
            })
    }

    fn discard_gpu(&mut self) -> Result<()> {
        self.converter.discard_export()
    }

    fn invalidate(&mut self) -> Result<()> {
        self.refresh = true;
        self.pending = true;
        self.cursor_pixels.clear();
        self.cpu_primary_dirty = true;
        self.cpu_cursor_dirty = true;
        self.cpu_frame_dirty = true;
        Ok(())
    }
}

fn claim_output(
    paths: &[PathBuf],
    request: &OutputRequest,
    cancel: &AtomicBool,
) -> Result<OwnedOutput> {
    let mut errors = Vec::new();
    for path in paths {
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "Hermes discovery cancelled"
        );
        let node = match Node::open(path) {
            Ok(node) => node,
            Err(error) => {
                if errors.len() < 4 {
                    errors.push(format!("{}: {error:#}", path.display()));
                }
                continue;
            }
        };
        let count = node.caps.output_count;
        drop(node);
        for index in 0..count {
            let result = (|| {
                let mut node = Node::open(path)?;
                node.select(index)?;
                node.claim(request)
            })();
            match result {
                Ok(output) => return Ok(output),
                Err(error) => {
                    if errors.len() < 8 {
                        errors.push(format!("{} output {index}: {error:#}", path.display()));
                    }
                }
            }
        }
    }
    anyhow::bail!("No compatible idle Hermes output: {}", errors.join("\n"))
}

fn overlay_cursor(
    pixels: &mut [u8],
    mode: Mode,
    cursor: &AcquiredCursor,
    source: &[u8],
) -> Result<()> {
    let image = cursor
        .image
        .as_ref()
        .context("Visible cursor has no image")?;
    ensure!(
        source.len() == image.width as usize * image.height as usize * 4,
        "Cursor readback has the wrong size"
    );
    ensure!(
        pixels.len() == mode.width as usize * mode.height as usize * 4,
        "Desktop readback has the wrong size"
    );
    let [x, y, width, height] = cursor.destination;
    let left = i64::from(x).max(0);
    let top = i64::from(y).max(0);
    let right = (i64::from(x) + i64::from(width)).min(i64::from(mode.width));
    let bottom = (i64::from(y) + i64::from(height)).min(i64::from(mode.height));
    for row in top..bottom {
        for column in left..right {
            let source_x = u64::from(cursor.source[0])
                + ((column - i64::from(x)) as u64 * 2 + 1) * u64::from(cursor.source[2])
                    / (width as u64 * 2);
            let source_y = u64::from(cursor.source[1])
                + ((row - i64::from(y)) as u64 * 2 + 1) * u64::from(cursor.source[3])
                    / (height as u64 * 2);
            let source_x = (source_x >> 16).min(u64::from(image.width - 1)) as usize;
            let source_y = (source_y >> 16).min(u64::from(image.height - 1)) as usize;
            let from = (source_y * image.width as usize + source_x) * 4;
            let to = (row as usize * mode.width as usize + column as usize) * 4;
            let alpha = u32::from(source[from + 3]);
            for channel in 0..3 {
                pixels[to + channel] = (u32::from(source[from + channel])
                    + (u32::from(pixels[to + channel]) * (255 - alpha) + 127) / 255)
                    .min(255) as u8;
            }
            pixels[to + 3] = 255;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmabuf::{DmaImage, Plane};

    #[test]
    fn clips_cursor_geometry_and_blends_premultiplied_pixels_without_moving_the_hotspot() {
        let mode = Mode {
            width: 3,
            height: 2,
            refresh_hz: 30,
        };
        let mut pixels = [100, 0, 20, 255].repeat(6);
        let cursor = AcquiredCursor {
            image: Some(DmaImage {
                buffer: crate::dmabuf::DmaBuffer {
                    width: 2,
                    height: 1,
                    format: PixelFormat::Argb8888,
                    fourcc: u32::from_le_bytes(*b"AR24"),
                    modifier: 0,
                    planes: vec![Plane {
                        descriptor: tempfile::tempfile().unwrap().into(),
                        pitch: 8,
                        offset: 0,
                        allocation_bytes: 8,
                    }],
                },
                fence: tempfile::tempfile().unwrap().into(),
            }),
            sequence: 2,
            image_sequence: 1,
            timestamp_ns: 0,
            visible: true,
            destination: [-1, 1, 2, 1],
            source: [0, 0, 2 << 16, 1 << 16],
        };
        overlay_cursor(
            &mut pixels,
            mode,
            &cursor,
            &[255, 0, 0, 255, 0, 128, 0, 128],
        )
        .unwrap();
        assert_eq!(&pixels[..12], &[100, 0, 20, 255].repeat(3));
        assert_eq!(&pixels[12..16], &[50, 128, 10, 255]);
        assert_eq!(&pixels[16..], &[100, 0, 20, 255].repeat(2));
    }
}
