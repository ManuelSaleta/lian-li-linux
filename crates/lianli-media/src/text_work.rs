use std::cell::Cell;

#[derive(Clone, Copy)]
struct Remaining {
    bytes: usize,
    pixels: u64,
    exceeded: bool,
}

thread_local! {
    // All text helpers on this rendering thread share one frame allowance.
    static WORK: Cell<Option<Remaining>> = const { Cell::new(None) };
}

pub(crate) struct FrameTextWork {
    previous: Option<Remaining>,
    same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl FrameTextWork {
    pub(crate) fn begin() -> Self {
        Self {
            previous: WORK.replace(Some(Remaining {
                bytes: 65_536,
                pixels: 32 * 1024 * 1024,
                exceeded: false,
            })),
            same_thread: std::marker::PhantomData,
        }
    }

    pub(crate) fn check(&self) -> Result<(), crate::MediaError> {
        if WORK.get().is_some_and(|work| work.exceeded) {
            return Err(crate::MediaError::InvalidConfig("Text rendering exceeds the per-frame work limit. Reduce text, font sizes or overlapping text widgets.".into()));
        }
        Ok(())
    }
}

impl Drop for FrameTextWork {
    fn drop(&mut self) {
        WORK.set(self.previous);
    }
}

fn charge(bytes: usize, pixels: u64) -> bool {
    WORK.with(|slot| {
        let Some(mut work) = slot.get() else {
            return true;
        };
        let allowed = !work.exceeded && bytes <= work.bytes && pixels <= work.pixels;
        if allowed {
            work.bytes -= bytes;
            work.pixels -= pixels;
        } else {
            work.exceeded = true;
        }
        slot.set(Some(work));
        allowed
    })
}

pub(crate) fn text(text: &str) -> bool {
    charge(text.len(), 0)
}

pub(crate) fn raster(width: u64, height: u64) -> bool {
    charge(0, width.saturating_mul(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_work_is_cumulative_and_failure_stays_latched() {
        let frame = FrameTextWork::begin();
        assert!(raster(4096, 4096));
        assert!(raster(4096, 4096));
        assert!(!raster(1, 1));
        assert!(!text(""));
        assert!(frame.check().is_err());
    }

    #[test]
    fn text_work_resets_after_error_and_is_local_to_each_thread() {
        {
            let frame = FrameTextWork::begin();
            assert!(text(&"x".repeat(65_536)));
            assert!(!text("x"));
            std::thread::spawn(|| {
                let other = FrameTextWork::begin();
                assert!(text("independent frame"));
                other.check().unwrap();
            })
            .join()
            .unwrap();
            assert!(frame.check().is_err());
        }
        let next = FrameTextWork::begin();
        assert!(text("next frame"));
        assert!(raster(480, 480));
        next.check().unwrap();
    }

    #[test]
    fn overflowing_raster_cost_cannot_wrap_into_the_budget() {
        let frame = FrameTextWork::begin();
        assert!(!raster(u64::MAX, 2));
        assert!(frame.check().is_err());
    }
}
