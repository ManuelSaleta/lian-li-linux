use super::runtime::{HidStreamWorker, LcdBackend, StreamRestarter};
use super::DaemonEvent;
use lianli_media::sensor::FrameInfo;
use lianli_media::video::LiveH264Encoder;
use lianli_media::{CustomAsset, MediaAsset, MediaAssetKind, SensorAsset};
use lianli_shared::screen::ScreenInfo;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Don't restart if the encoder crashed within this period — likely systemic.
const MIN_HEALTHY_UPTIME: Duration = Duration::from_secs(10);
/// Max restart attempts before giving up.
const MAX_RESTARTS: u32 = 3;
/// Reset the restart counter after this long healthy streak.
const HEALTHY_RESET: Duration = Duration::from_secs(300);

fn wait_until_stopped(stop: &AtomicBool, deadline: Instant) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::park_timeout(remaining);
    }
}

struct EncoderSettings<'a> {
    width: u32,
    height: u32,
    fps: f32,
    rotation_deg: u16,
    screen: &'a ScreenInfo,
    hardware_video: bool,
}

fn encoder_restart_backoff(restart_count: &mut u32, uptime: Duration) -> Option<Duration> {
    if uptime > HEALTHY_RESET {
        *restart_count = 0;
    }
    if *restart_count >= MAX_RESTARTS {
        warn!("h264 encoder exceeded max restarts ({MAX_RESTARTS}), giving up");
        return None;
    }

    if uptime < MIN_HEALTHY_UPTIME {
        warn!(
            "h264 encoder only ran {:?} (< {MIN_HEALTHY_UPTIME:?}), not restarting",
            uptime
        );
        return None;
    }

    *restart_count += 1;
    let backoff_secs = 2u64.pow(restart_count.saturating_sub(1)).min(30);
    info!(
        "restarting h264 encoder (attempt {}/{MAX_RESTARTS}) after {backoff_secs}s backoff",
        *restart_count
    );
    Some(Duration::from_secs(backoff_secs))
}

fn try_restart_encoder(
    encoder: &mut LiveH264Encoder,
    encoder_status: &Mutex<lianli_shared::ipc::MediaEncoderStatus>,
    restarter: &StreamRestarter,
    stop: &Arc<AtomicBool>,
    settings: EncoderSettings<'_>,
    restart_count: &mut u32,
    started_at: &mut Instant,
) -> bool {
    if stop.load(Ordering::Relaxed) {
        return false;
    }
    let Some(backoff) = encoder_restart_backoff(restart_count, started_at.elapsed()) else {
        return false;
    };
    if wait_until_stopped(stop, Instant::now() + backoff) {
        return false;
    }

    let EncoderSettings {
        width,
        height,
        fps,
        rotation_deg,
        screen,
        hardware_video,
    } = settings;
    let mut new_encoder =
        match LiveH264Encoder::spawn(width, height, fps, rotation_deg, screen, hardware_video) {
            Ok(enc) => enc,
            Err(e) => {
                warn!("h264 encoder respawn failed: {e}");
                return false;
            }
        };

    if stop.load(Ordering::Relaxed) {
        return false;
    }
    if let Some(stdout) = new_encoder.take_stdout() {
        if let Err(e) = restarter.start_stream(stdout, Arc::clone(stop), fps) {
            warn!("h264 stream restart failed: {e}");
            return false;
        }
    }

    let status = new_encoder.encoder_status(hardware_video);
    *encoder = new_encoder;
    *encoder_status.lock() = status;
    *started_at = Instant::now();
    info!("h264 encoder restarted successfully");
    true
}

pub(super) struct AsyncSensorRenderer {
    current_frame: Arc<Mutex<FrameInfo>>,
    stop_flag: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
}

fn empty_jpeg_frame() -> Arc<Mutex<FrameInfo>> {
    Arc::new(Mutex::new(FrameInfo {
        data: Vec::new(),
        frame_index: 0,
    }))
}

fn initialize_jpeg_frame(
    frame: &Mutex<FrameInfo>,
    stop: &AtomicBool,
    render: impl FnOnce() -> anyhow::Result<FrameInfo>,
) -> bool {
    if stop.load(Ordering::Relaxed) {
        return false;
    }
    match render() {
        Ok(initial) if !stop.load(Ordering::Relaxed) => {
            *frame.lock() = initial;
            true
        }
        Ok(_) => false,
        Err(error) => {
            warn!("Initial JPEG render failed: {error:#}");
            stop.store(true, Ordering::Relaxed);
            false
        }
    }
}

impl AsyncSensorRenderer {
    pub(super) fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(worker) = &self._thread {
            worker.thread().unpark();
        }
    }

    pub(super) fn retirement_complete(&self) -> bool {
        self._thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) fn new(
        tx: Option<Sender<DaemonEvent>>,
        asset: Arc<SensorAsset>,
        baseasset: Arc<MediaAsset>,
        keep_alive_on_no_change: bool,
    ) -> Self {
        let current_frame = empty_jpeg_frame();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let update_interval = asset.update_interval();

        let asset_clone = Arc::clone(&asset);
        let frame_clone = Arc::clone(&current_frame);
        let stop_clone = Arc::clone(&stop_flag);

        let _asset_for_thread = Arc::clone(&baseasset);
        let tx_for_thread = tx.clone();

        let thread = thread::spawn(move || {
            if !initialize_jpeg_frame(&frame_clone, &stop_clone, || {
                Ok(asset_clone
                    .render_frame(true)?
                    .unwrap_or_else(|| asset_clone.blank_frame()))
            }) {
                return;
            }
            if let Some(tx) = &tx_for_thread {
                if tx.send(DaemonEvent::FrameFinished).is_err() {
                    return;
                }
            }
            while !stop_clone.load(Ordering::Relaxed) {
                if wait_until_stopped(&stop_clone, Instant::now() + update_interval) {
                    break;
                }
                match asset_clone.render_frame(keep_alive_on_no_change) {
                    Ok(Some(new_frame)) => {
                        *frame_clone.lock() = new_frame;
                    }
                    Ok(None) => {
                        frame_clone.lock().frame_index += 1;
                    }
                    Err(err) => {
                        warn!("sensor background render failed: {err}");
                        stop_clone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                if let Some(ref tx) = tx_for_thread {
                    let event = DaemonEvent::FrameFinished;
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            current_frame,
            stop_flag,
            _thread: Some(thread),
        }
    }

    pub(super) fn has_exited(&self) -> bool {
        self.stop_flag.load(Ordering::Relaxed)
            || self._thread.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub(super) fn get_frame_index(&self) -> usize {
        self.current_frame.lock().frame_index
    }

    pub(super) fn get_current_frame(&self) -> Vec<u8> {
        self.current_frame.lock().data.clone()
    }
}

impl Drop for AsyncSensorRenderer {
    fn drop(&mut self) {
        self.request_stop();
        if self.retirement_complete() {
            if let Some(worker) = self._thread.take() {
                if worker.join().is_err() {
                    warn!("LCD renderer panicked during retirement");
                }
            }
        }
    }
}

pub(super) struct AsyncVideoPlayer {
    stop_flag: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
    frame_index: Arc<AtomicUsize>,
}

impl AsyncVideoPlayer {
    pub(super) fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(worker) = &self._thread {
            worker.thread().unpark();
        }
    }

    pub(super) fn retirement_complete(&self) -> bool {
        self._thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) fn new(tx: Option<Sender<DaemonEvent>>, asset: Arc<MediaAsset>) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_flag);

        let tx_for_thread = tx.clone();

        let _asset_for_thread = Arc::clone(&asset);

        let min_dur = Duration::from_millis(10);
        let std_dur = Duration::from_millis(100);

        let frame_durations: Vec<Duration> = if let MediaAssetKind::Video {
            frame_durations, ..
        } = &asset.kind
        {
            frame_durations.iter().map(|&d| d.max(min_dur)).collect()
        } else {
            vec![min_dur; 1]
        };

        let frame_index: Arc<AtomicUsize> = Arc::new(0.into());
        let frame_index_cloned = frame_index.clone();

        let thread = thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                let mut frame_cnt = 0;
                if let Some(ref tx) = tx_for_thread {
                    frame_cnt = frame_index.fetch_add(1, Ordering::SeqCst);
                    let event = DaemonEvent::FrameFinished;
                    if tx.send(event).is_err() {
                        break;
                    }
                }

                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }

                let millis = frame_durations.get(frame_cnt % frame_durations.len());
                if wait_until_stopped(&stop_clone, Instant::now() + *millis.unwrap_or(&std_dur)) {
                    break;
                }
            }
        });

        Self {
            stop_flag,
            _thread: Some(thread),
            frame_index: frame_index_cloned,
        }
    }

    pub(super) fn get_frame_index(&self) -> usize {
        self.frame_index.load(Ordering::SeqCst)
    }
}

impl Drop for AsyncVideoPlayer {
    fn drop(&mut self) {
        self.request_stop();
        if self.retirement_complete() {
            if let Some(worker) = self._thread.take() {
                if worker.join().is_err() {
                    warn!("LCD renderer panicked during retirement");
                }
            }
        }
    }
}

pub(super) struct AsyncCustomRenderer {
    current_frame: Arc<Mutex<FrameInfo>>,
    stop_flag: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
}

impl AsyncCustomRenderer {
    pub(super) fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(worker) = &self._thread {
            worker.thread().unpark();
        }
    }

    pub(super) fn retirement_complete(&self) -> bool {
        self._thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) fn has_exited(&self) -> bool {
        self.stop_flag.load(Ordering::Relaxed)
            || self._thread.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub(super) fn new(
        tx: Option<Sender<DaemonEvent>>,
        asset: Arc<CustomAsset>,
        baseasset: Arc<MediaAsset>,
        keep_alive_on_no_change: bool,
    ) -> Self {
        let current_frame = empty_jpeg_frame();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let update_interval = asset.update_interval();

        let asset_clone = Arc::clone(&asset);
        let frame_clone = Arc::clone(&current_frame);
        let stop_clone = Arc::clone(&stop_flag);

        let _asset_for_thread = Arc::clone(&baseasset);
        let tx_for_thread = tx.clone();

        let thread = thread::spawn(move || {
            if !initialize_jpeg_frame(&frame_clone, &stop_clone, || {
                Ok(asset_clone
                    .render_frame(true)?
                    .unwrap_or_else(|| asset_clone.blank_frame()))
            }) {
                return;
            }
            if let Some(tx) = &tx_for_thread {
                if tx.send(DaemonEvent::FrameFinished).is_err() {
                    return;
                }
            }
            let mut next_deadline = Instant::now() + update_interval;
            while !stop_clone.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next_deadline && wait_until_stopped(&stop_clone, next_deadline) {
                    break;
                }
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                next_deadline += update_interval;
                if next_deadline < Instant::now() {
                    next_deadline = Instant::now() + update_interval;
                }
                match asset_clone.render_frame(keep_alive_on_no_change) {
                    Ok(Some(new_frame)) => {
                        *frame_clone.lock() = new_frame;
                        if let Some(ref tx) = tx_for_thread {
                            let event = DaemonEvent::FrameFinished;
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!("Custom background render failed: {err}");
                        stop_clone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });

        Self {
            current_frame,
            stop_flag,
            _thread: Some(thread),
        }
    }

    pub(super) fn get_frame_index(&self) -> usize {
        self.current_frame.lock().frame_index
    }

    pub(super) fn get_current_frame(&self) -> Vec<u8> {
        self.current_frame.lock().data.clone()
    }
}

impl Drop for AsyncCustomRenderer {
    fn drop(&mut self) {
        self.request_stop();
        if self.retirement_complete() {
            if let Some(worker) = self._thread.take() {
                if worker.join().is_err() {
                    warn!("LCD renderer panicked during retirement");
                }
            }
        }
    }
}

pub(super) struct PreparedCustomH264 {
    asset: Arc<CustomAsset>,
    encoder: LiveH264Encoder,
    screen: ScreenInfo,
    fps: f32,
    hardware_video: bool,
}

impl PreparedCustomH264 {
    pub fn prepare(
        asset: Arc<CustomAsset>,
        screen: ScreenInfo,
        fps: f32,
        hardware_video: bool,
    ) -> anyhow::Result<Self> {
        let fps = lianli_media::video::h264::frame_rate(fps, &screen) as f32;
        let encoder = LiveH264Encoder::spawn(
            asset.canvas_width(),
            asset.canvas_height(),
            fps,
            asset.total_rotation_deg(),
            &screen,
            hardware_video,
        )
        .map_err(|e| anyhow::anyhow!("h264 encoder spawn: {e}"))?;
        Ok(Self {
            asset,
            encoder,
            screen,
            fps,
            hardware_video,
        })
    }

    pub fn start(
        self,
        lcd: &LcdBackend,
    ) -> Result<AsyncCustomH264Renderer, (Box<Self>, anyhow::Error)> {
        AsyncCustomH264Renderer::start(self, lcd)
    }
}

pub(super) struct AsyncCustomH264Renderer {
    restarter: Arc<StreamRestarter>,
    encoder_status: Arc<Mutex<lianli_shared::ipc::MediaEncoderStatus>>,
    stop_flag: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
    _stream_thread: Option<HidStreamWorker>,
}

impl AsyncCustomH264Renderer {
    pub(super) fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(worker) = &self._thread {
            worker.thread().unpark();
        }
    }

    pub(super) fn retirement_complete(&self) -> bool {
        self._thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) fn transferred(&self) -> Option<bool> {
        self.restarter.transferred()
    }
    pub(super) fn encoder_status(&self) -> lianli_shared::ipc::MediaEncoderStatus {
        self.encoder_status.lock().clone()
    }
    pub(super) fn has_exited(&self) -> bool {
        self._thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn start(
        mut prepared: PreparedCustomH264,
        lcd: &LcdBackend,
    ) -> Result<Self, (Box<PreparedCustomH264>, anyhow::Error)> {
        let Some(stdout) = prepared.encoder.take_stdout() else {
            return Err((
                Box::new(prepared),
                anyhow::anyhow!("H.264 encoder stdout missing"),
            ));
        };
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut stream_thread = match lcd.start_h264_stream(stdout, stop_flag.clone(), prepared.fps)
        {
            Ok(stream) => stream,
            Err(error) => return Err((Box::new(prepared), error)),
        };
        let Some(restarter) = lcd.stream_restarter(stream_thread.take()) else {
            stop_flag.store(true, Ordering::Relaxed);
            return Err((
                Box::new(prepared),
                anyhow::anyhow!("H.264 restart is unavailable"),
            ));
        };
        let restarter = Arc::new(restarter);
        let PreparedCustomH264 {
            asset,
            mut encoder,
            screen,
            fps,
            hardware_video,
        } = prepared;
        let canvas_w = asset.canvas_width();
        let canvas_h = asset.canvas_height();
        let rotation_deg = asset.total_rotation_deg();
        let stop_clone = Arc::clone(&stop_flag);
        let encoder_status = Arc::new(Mutex::new(encoder.encoder_status(hardware_video)));
        let status_clone = encoder_status.clone();
        let frame_interval =
            Duration::from_secs_f32(1.0 / fps.max(1.0)).max(Duration::from_millis(16));

        let shared_restarter = restarter.clone();
        let screen_clone = screen;

        let thread = thread::spawn(move || {
            let mut next_deadline = Instant::now() + frame_interval;
            let mut restart_count = 0u32;
            let mut encoder_started_at = Instant::now();
            while !stop_clone.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next_deadline && wait_until_stopped(&stop_clone, next_deadline) {
                    break;
                }
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                next_deadline += frame_interval;
                if next_deadline < Instant::now() {
                    next_deadline = Instant::now() + frame_interval;
                }

                match restarter.try_start_pending() {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        warn!("Custom H.264 transfer stopped: {error:#}");
                        break;
                    }
                }
                let outcome = asset.render_frame_rgba_with(true, |rgba| encoder.write_frame(rgba));
                match outcome {
                    Ok(Some(Ok(()))) => {}
                    Ok(Some(Err(e))) => {
                        warn!("custom h264 encoder write failed: {e}");
                        if !try_restart_encoder(
                            &mut encoder,
                            &status_clone,
                            &restarter,
                            &stop_clone,
                            EncoderSettings {
                                width: canvas_w,
                                height: canvas_h,
                                fps,
                                rotation_deg,
                                screen: &screen_clone,
                                hardware_video,
                            },
                            &mut restart_count,
                            &mut encoder_started_at,
                        ) {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!("custom h264 render failed: {err}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            stop_flag,
            encoder_status,
            restarter: shared_restarter,
            _thread: Some(thread),
            _stream_thread: stream_thread,
        })
    }
}

impl Drop for AsyncCustomH264Renderer {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self._thread.take() {
            t.thread().unpark();
            finish_h264_renderer(t);
        }
    }
}

pub(super) struct PreparedSensorH264 {
    asset: Arc<SensorAsset>,
    initial: image::RgbaImage,
    encoder: LiveH264Encoder,
    screen: ScreenInfo,
    fps: f32,
    hardware_video: bool,
}

impl PreparedSensorH264 {
    pub fn prepare(
        asset: Arc<SensorAsset>,
        screen: ScreenInfo,
        fps: f32,
        hardware_video: bool,
    ) -> anyhow::Result<Self> {
        let fps = lianli_media::video::h264::frame_rate(fps, &screen) as f32;
        let initial = asset
            .render_frame_rgba(true)?
            .ok_or_else(|| anyhow::anyhow!("sensor produced no initial frame"))?;
        let encoder = LiveH264Encoder::spawn(
            initial.width(),
            initial.height(),
            fps,
            0,
            &screen,
            hardware_video,
        )
        .map_err(|e| anyhow::anyhow!("h264 encoder spawn: {e}"))?;
        Ok(Self {
            asset,
            initial,
            encoder,
            screen,
            fps,
            hardware_video,
        })
    }

    pub fn start(
        self,
        lcd: &LcdBackend,
    ) -> Result<AsyncSensorH264Renderer, (Box<Self>, anyhow::Error)> {
        AsyncSensorH264Renderer::start(self, lcd)
    }
}

pub(super) struct AsyncSensorH264Renderer {
    restarter: Arc<StreamRestarter>,
    encoder_status: Arc<Mutex<lianli_shared::ipc::MediaEncoderStatus>>,
    stop_flag: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
    _stream_thread: Option<HidStreamWorker>,
}

impl AsyncSensorH264Renderer {
    pub(super) fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(worker) = &self._thread {
            worker.thread().unpark();
        }
    }

    pub(super) fn retirement_complete(&self) -> bool {
        self._thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(super) fn transferred(&self) -> Option<bool> {
        self.restarter.transferred()
    }
    pub(super) fn encoder_status(&self) -> lianli_shared::ipc::MediaEncoderStatus {
        self.encoder_status.lock().clone()
    }
    pub(super) fn has_exited(&self) -> bool {
        self._thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn start(
        mut prepared: PreparedSensorH264,
        lcd: &LcdBackend,
    ) -> Result<Self, (Box<PreparedSensorH264>, anyhow::Error)> {
        let Some(stdout) = prepared.encoder.take_stdout() else {
            return Err((
                Box::new(prepared),
                anyhow::anyhow!("H.264 encoder stdout missing"),
            ));
        };
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut stream_thread = match lcd.start_h264_stream(stdout, stop_flag.clone(), prepared.fps)
        {
            Ok(stream) => stream,
            Err(error) => return Err((Box::new(prepared), error)),
        };
        let Some(restarter) = lcd.stream_restarter(stream_thread.take()) else {
            stop_flag.store(true, Ordering::Relaxed);
            return Err((
                Box::new(prepared),
                anyhow::anyhow!("H.264 restart is unavailable"),
            ));
        };
        let restarter = Arc::new(restarter);
        let PreparedSensorH264 {
            asset,
            initial,
            mut encoder,
            screen,
            fps,
            hardware_video,
        } = prepared;
        let canvas_w = initial.width();
        let canvas_h = initial.height();
        let stop_clone = Arc::clone(&stop_flag);
        let encoder_status = Arc::new(Mutex::new(encoder.encoder_status(hardware_video)));
        let status_clone = encoder_status.clone();
        let frame_interval =
            Duration::from_secs_f32(1.0 / fps.max(1.0)).max(Duration::from_millis(16));

        let shared_restarter = restarter.clone();
        let screen_clone = screen;

        let thread = thread::spawn(move || {
            let mut initial = Some(initial);
            let mut next_deadline = Instant::now();
            let mut restart_count = 0u32;
            let mut encoder_started_at = Instant::now();
            while !stop_clone.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next_deadline && wait_until_stopped(&stop_clone, next_deadline) {
                    break;
                }
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                next_deadline += frame_interval;
                if next_deadline < Instant::now() {
                    next_deadline = Instant::now() + frame_interval;
                }

                match restarter.try_start_pending() {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        warn!("Sensor H.264 transfer stopped: {error:#}");
                        break;
                    }
                }
                let frame = match initial.take() {
                    Some(frame) => Ok(Some(frame)),
                    None => asset.render_frame_rgba(true),
                };
                match frame {
                    Ok(Some(rgba)) => {
                        if let Err(e) = encoder.write_frame(rgba.as_raw()) {
                            warn!("sensor h264 encoder write failed: {e}");
                            if !try_restart_encoder(
                                &mut encoder,
                                &status_clone,
                                &restarter,
                                &stop_clone,
                                EncoderSettings {
                                    width: canvas_w,
                                    height: canvas_h,
                                    fps,
                                    rotation_deg: 0,
                                    screen: &screen_clone,
                                    hardware_video,
                                },
                                &mut restart_count,
                                &mut encoder_started_at,
                            ) {
                                break;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!("sensor h264 render failed: {err}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            stop_flag,
            encoder_status,
            restarter: shared_restarter,
            _thread: Some(thread),
            _stream_thread: stream_thread,
        })
    }
}

impl Drop for AsyncSensorH264Renderer {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self._thread.take() {
            t.thread().unpark();
            finish_h264_renderer(t);
        }
    }
}

fn finish_h264_renderer(worker: JoinHandle<()>) {
    let deadline = Instant::now() + Duration::from_millis(100);
    while !worker.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if worker.is_finished() {
        let _ = worker.join();
    } else {
        // The stopped worker owns its encoder until its bounded pipe write finishes.
        warn!("H.264 renderer is stopping; detaching until its current frame completes");
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn initial_jpeg_failure_and_cancellation_never_publish_a_frame() {
        let frame = empty_jpeg_frame();
        let stop = AtomicBool::new(true);
        assert!(!initialize_jpeg_frame(&frame, &stop, || panic!(
            "cancelled render"
        )));
        stop.store(false, Ordering::Relaxed);
        assert!(!initialize_jpeg_frame(&frame, &stop, || anyhow::bail!(
            "fixture failure"
        )));
        assert!(stop.load(Ordering::Relaxed));
        assert_eq!(frame.lock().frame_index, 0);
        assert!(frame.lock().data.is_empty());
        stop.store(false, Ordering::Relaxed);
        assert!(!initialize_jpeg_frame(&frame, &stop, || {
            stop.store(true, Ordering::Relaxed);
            Ok(FrameInfo {
                data: vec![1],
                frame_index: 1,
            })
        }));
        assert_eq!(frame.lock().frame_index, 0);
        assert!(frame.lock().data.is_empty());
    }

    #[test]
    fn sensor_initial_jpeg_wakes_streaming_before_its_refresh_interval() {
        let descriptor = serde_json::from_value(serde_json::json!({
            "label": "Test", "unit": "%", "source": { "type": "constant", "value": 25 }
        }))
        .unwrap();
        let sensor =
            SensorAsset::new(&descriptor, 0.0, &ScreenInfo::TLLCD, &[], None, 60_000).unwrap();
        let asset = Arc::new(MediaAsset {
            kind: MediaAssetKind::Sensor {
                asset: sensor.clone(),
            },
            config_key: "initial-jpeg".into(),
            stream_fps: 30.0,
            hardware_video: false,
        });
        let (tx, rx) = mpsc::channel();
        let renderer = AsyncSensorRenderer::new(Some(tx), sensor, asset, false);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            DaemonEvent::FrameFinished
        ));
        assert!(renderer.get_frame_index() > 0);
        let bytes = renderer.get_current_frame();
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
        assert!(!renderer.has_exited());
    }

    #[test]
    fn healthy_encoder_restores_an_exhausted_restart_budget() {
        let mut attempts = MAX_RESTARTS;
        assert_eq!(encoder_restart_backoff(&mut attempts, HEALTHY_RESET), None);
        assert_eq!(
            encoder_restart_backoff(&mut attempts, HEALTHY_RESET + Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(attempts, 1);
    }

    #[test]
    fn repeated_encoder_failures_keep_bounded_backoff() {
        let mut attempts = 0;
        assert_eq!(encoder_restart_backoff(&mut attempts, Duration::ZERO), None);
        assert_eq!(attempts, 0);
        for seconds in [1, 2, 4] {
            assert_eq!(
                encoder_restart_backoff(&mut attempts, MIN_HEALTHY_UPTIME),
                Some(Duration::from_secs(seconds))
            );
        }
        assert_eq!(
            encoder_restart_backoff(&mut attempts, MIN_HEALTHY_UPTIME),
            None
        );
        assert_eq!(attempts, MAX_RESTARTS);
    }

    #[test]
    fn dropping_player_wakes_a_long_frame_wait() {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let stopped =
                wait_until_stopped(&worker_stop, Instant::now() + Duration::from_secs(60));
            done_tx.send(stopped).unwrap();
        });
        let wake = worker.thread().clone();
        let player = AsyncVideoPlayer {
            stop_flag: stop,
            _thread: Some(worker),
            frame_index: Arc::new(AtomicUsize::new(0)),
        };
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(player);
        let result = done_rx.recv_timeout(Duration::from_secs(1));
        wake.unpark();
        assert!(result.unwrap());
    }

    #[test]
    fn an_unrelated_wakeup_does_not_advance_the_frame_deadline() {
        let stop = AtomicBool::new(false);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(40);
            thread::current().unpark();
            assert!(!wait_until_stopped(&stop, deadline));
            assert!(Instant::now() >= deadline);
        });
        worker.join().unwrap();
    }
}
