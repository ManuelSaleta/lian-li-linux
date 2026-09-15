use super::renderers::{PreparedCustomH264, PreparedSensorH264};
use super::runtime::{make_jpeg_source, FrameSource};
use super::DaemonEvent;
use lianli_media::{MediaAsset, MediaAssetKind};
use lianli_shared::screen::ScreenInfo;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) struct SourceRequest {
    pub index: usize,
    pub selection: Weak<()>,
    pub asset: Arc<MediaAsset>,
    pub screen: ScreenInfo,
    pub custom_h264: bool,
    pub tx: Option<mpsc::Sender<DaemonEvent>>,
}

pub(super) enum PreparedSource {
    Sensor(Box<PreparedSensorH264>),
    Custom(Box<PreparedCustomH264>),
    Jpeg {
        source: Box<dyn FrameSource>,
        fallback: Option<String>,
    },
}

pub(super) struct SourceResult {
    pub index: usize,
    pub selection: Weak<()>,
    pub result: PreparedOffer<Result<PreparedSource, String>>,
}

pub(super) struct PreparedOffer<T> {
    value: Option<T>,
    rejected: Option<mpsc::SyncSender<T>>,
}

impl<T> From<T> for PreparedOffer<T> {
    fn from(value: T) -> Self {
        Self {
            value: Some(value),
            rejected: None,
        }
    }
}

impl<T> PreparedOffer<T> {
    fn returned_to(value: T, rejected: mpsc::SyncSender<T>) -> Self {
        Self {
            value: Some(value),
            rejected: Some(rejected),
        }
    }

    #[cfg(test)]
    pub fn accept(mut self) -> T {
        self.value.take().unwrap()
    }

    pub fn try_accept<R, E>(mut self, accept: impl FnOnce(T) -> Result<R, (T, E)>) -> Result<R, E> {
        match accept(self.value.take().unwrap()) {
            Ok(value) => Ok(value),
            Err((value, error)) => {
                self.value = Some(value);
                Err(error)
            }
        }
    }
}

impl<T> Drop for PreparedOffer<T> {
    fn drop(&mut self) {
        if let (Some(value), Some(rejected)) = (self.value.take(), self.rejected.take()) {
            // One offer has one return slot. Its worker waits until acceptance or return.
            let _ = rejected.send(value);
        }
    }
}

struct Running {
    index: usize,
    selection: Weak<()>,
    cancel: Arc<AtomicBool>,
    receiver: Receiver<SourceResult>,
    thread: JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct SourcePreparation {
    running: Option<Running>,
}

impl SourcePreparation {
    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    pub fn start(&mut self, request: SourceRequest) -> std::io::Result<bool> {
        self.start_with(request, prepare)
    }

    fn start_with(
        &mut self,
        request: SourceRequest,
        prepare: impl FnOnce(&SourceRequest, &AtomicBool) -> Result<PreparedSource, String>
            + Send
            + 'static,
    ) -> std::io::Result<bool> {
        if self.is_busy() {
            return Ok(false);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let index = request.index;
        let selection = request.selection.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("lcd-source-prepare".into())
            .spawn(move || {
                let cancelled = || {
                    worker_cancel.load(Ordering::Acquire) || request.selection.strong_count() == 0
                };
                if cancelled() {
                    return;
                }
                let result = prepare(&request, &worker_cancel);
                if cancelled() {
                    return;
                }
                let (rejected, returned) = mpsc::sync_channel(1);
                let _ = sender.send(SourceResult {
                    index: request.index,
                    selection: request.selection,
                    result: PreparedOffer::returned_to(result, rejected),
                });
                drop(returned.recv());
            })?;
        self.running = Some(Running {
            index,
            selection,
            cancel,
            receiver,
            thread,
        });
        Ok(true)
    }

    pub fn poll(&mut self) -> Option<SourceResult> {
        let running = self.running.as_ref()?;
        let mut result = running.receiver.try_recv().ok();
        if running.thread.is_finished() {
            let running = self.running.take().unwrap();
            let failed = running.thread.join().is_err();
            if result.is_none() {
                result = running.receiver.try_recv().ok();
            }
            if failed && result.is_none() {
                result = Some(SourceResult {
                    index: running.index,
                    selection: running.selection,
                    result: Err("Media source preparation failed. Retry failed media.".into())
                        .into(),
                });
            }
        }
        result.filter(|result| result.selection.strong_count() > 0)
    }
}

impl Drop for SourcePreparation {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            running.cancel.store(true, Ordering::Release);
            drop(running.receiver);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !running.thread.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if running.thread.is_finished() {
                if running.thread.join().is_err() {
                    tracing::warn!("Media source preparation panicked during shutdown");
                }
            } else {
                // Construction owns no device handles and finishes cleanup on its worker.
                tracing::warn!("Media source preparation is finishing a cancelled operation");
            }
        }
    }
}

fn prepare(request: &SourceRequest, cancel: &AtomicBool) -> Result<PreparedSource, String> {
    let asset = &request.asset;
    let result = match &asset.kind {
        MediaAssetKind::Sensor { asset: sensor } if request.screen.h264 => {
            PreparedSensorH264::prepare(
                sensor.clone(),
                request.screen,
                asset.stream_fps,
                asset.hardware_video,
            )
            .map(|source| PreparedSource::Sensor(Box::new(source)))
        }
        MediaAssetKind::Custom { asset: custom } if request.screen.h264 && request.custom_h264 => {
            PreparedCustomH264::prepare(
                custom.clone(),
                request.screen,
                asset.stream_fps,
                asset.hardware_video,
            )
            .map(|source| PreparedSource::Custom(Box::new(source)))
        }
        MediaAssetKind::Sensor { .. } | MediaAssetKind::Custom { .. } => {
            return prepare_jpeg(request, cancel, None)
        }
        _ => return Err("This media source does not need background preparation".into()),
    };
    match result {
        Ok(source) => Ok(source),
        Err(error) => prepare_jpeg(
            request,
            cancel,
            Some(
                format!("H.264 unavailable. Using JPEG: {error:#}")
                    .chars()
                    .take(2048)
                    .collect(),
            ),
        ),
    }
}

fn prepare_jpeg(
    request: &SourceRequest,
    cancel: &AtomicBool,
    fallback: Option<String>,
) -> Result<PreparedSource, String> {
    let mut source = make_jpeg_source(request.asset.clone(), request.tx.clone(), &request.screen)
        .ok_or_else(|| "This media source does not use a JPEG renderer".to_owned())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cancel.load(Ordering::Acquire) || request.selection.strong_count() == 0 {
            return Err("Media preparation cancelled".into());
        }
        if source.has_exited() {
            return Err("Initial JPEG rendering failed. Check daemon logs.".into());
        }
        if source.next_frame().is_some_and(|frame| !frame.is_empty()) {
            return Ok(PreparedSource::Jpeg { source, fallback });
        }
        if Instant::now() >= deadline {
            return Err("Initial JPEG rendering timed out. Retry failed media.".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(selection: &Arc<()>) -> SourceRequest {
        SourceRequest {
            index: 7,
            selection: Arc::downgrade(selection),
            screen: ScreenInfo::TLLCD,
            custom_h264: false,
            tx: None,
            asset: Arc::new(MediaAsset {
                kind: MediaAssetKind::Static {
                    frame: lianli_media::Retained::frame(vec![]).unwrap(),
                },
                config_key: "fixture".into(),
                stream_fps: 30.0,
                hardware_video: false,
            }),
        }
    }

    fn finish(worker: &mut SourcePreparation) -> Option<SourceResult> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.is_busy() && Instant::now() < deadline {
            if let Some(value) = worker.poll() {
                return Some(value);
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!worker.is_busy());
        None
    }

    #[test]
    fn busy_worker_rejects_an_overlapping_job_and_delivers_original_result() {
        let selection = Arc::new(());
        let mut worker = SourcePreparation::default();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        assert!(worker
            .start_with(request(&selection), move |_, _| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                Err("fixture failure".into())
            })
            .unwrap());
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!worker
            .start_with(request(&selection), |_, _| panic!("overlapping job"))
            .unwrap());
        release_tx.send(()).unwrap();
        let result = finish(&mut worker).unwrap();
        assert_eq!(result.index, 7);
        assert!(Weak::ptr_eq(&result.selection, &Arc::downgrade(&selection)));
        assert_eq!(result.result.accept().err().unwrap(), "fixture failure");
        assert!(finish(&mut worker).is_none());
    }

    #[test]
    fn removed_selection_discards_in_flight_output() {
        let selection = Arc::new(());
        let mut worker = SourcePreparation::default();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        worker
            .start_with(request(&selection), move |_, _| {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                Err("obsolete failure".into())
            })
            .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(selection);
        release_tx.send(()).unwrap();
        assert!(finish(&mut worker).is_none());
    }

    #[test]
    fn queued_output_is_discarded_after_its_selection_is_removed() {
        let selection = Arc::new(());
        let mut worker = SourcePreparation::default();
        worker.start(request(&selection)).unwrap();
        let result = finish(&mut worker).unwrap();
        drop(selection);
        drop(result);
        assert!(finish(&mut worker).is_none());
    }

    #[test]
    fn source_errors_are_delivered_without_a_device() {
        let selection = Arc::new(());
        let mut worker = SourcePreparation::default();
        assert!(worker.start(request(&selection)).unwrap());
        assert_eq!(
            finish(&mut worker).unwrap().result.accept().err().unwrap(),
            "This media source does not need background preparation"
        );
    }

    #[test]
    fn jpeg_preparation_delivers_a_usable_unsent_frame() {
        let selection = Arc::new(());
        let mut request = request(&selection);
        let descriptor = serde_json::from_value(serde_json::json!({
            "label": "Test", "unit": "%", "source": { "type": "constant", "value": 25 }
        }))
        .unwrap();
        request.screen.h264 = false;
        let sensor =
            lianli_media::SensorAsset::new(&descriptor, 0.0, &request.screen, &[], None, 60_000)
                .unwrap();
        request.asset = Arc::new(MediaAsset {
            kind: MediaAssetKind::Sensor { asset: sensor },
            config_key: "jpeg-ready".into(),
            stream_fps: 30.0,
            hardware_video: false,
        });
        let mut worker = SourcePreparation::default();
        worker.start(request).unwrap();
        let PreparedSource::Jpeg {
            mut source,
            fallback,
        } = finish(&mut worker).unwrap().result.accept().ok().unwrap()
        else {
            panic!("expected prepared JPEG");
        };
        assert!(fallback.is_none());
        assert_eq!(&source.next_frame().unwrap()[..2], &[0xff, 0xd8]);
        source.mark_sent();
        assert!(source.next_frame().is_none());
        assert!(finish(&mut worker).is_none());
    }

    #[test]
    fn rejected_offer_cleans_up_on_its_worker_without_waiting_on_the_caller() {
        for attempt_attachment in [false, true] {
            struct SlowDrop {
                started: mpsc::Sender<thread::ThreadId>,
                release: Receiver<()>,
            }
            impl Drop for SlowDrop {
                fn drop(&mut self) {
                    self.started.send(thread::current().id()).unwrap();
                    self.release.recv_timeout(Duration::from_secs(2)).unwrap();
                }
            }
            let (offer_tx, offer_rx) = mpsc::sync_channel(1);
            let (started, cleanup_started) = mpsc::channel();
            let (release, cleanup_release) = mpsc::channel();
            let worker = thread::spawn(move || {
                let (rejected, returned) = mpsc::sync_channel(1);
                let value = SlowDrop {
                    started,
                    release: cleanup_release,
                };
                offer_tx
                    .send(PreparedOffer::returned_to(value, rejected))
                    .ok()
                    .unwrap();
                drop(returned.recv());
            });
            let offer = offer_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            if attempt_attachment {
                assert!(offer.try_accept(|value| Err::<(), _>((value, ()))).is_err());
            } else {
                drop(offer);
            }
            let cleanup_thread = cleanup_started
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            release.send(()).unwrap();
            assert_eq!(cleanup_thread, worker.thread().id());
            worker.join().unwrap();
        }
    }
}
