use anyhow::{ensure, Result};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[derive(Default)]
pub struct Transfer {
    state: AtomicU8,
}

impl Transfer {
    pub fn begin(&self, stop: &AtomicBool) -> Result<()> {
        ensure_not_cancelled(stop)?;
        ensure!(
            self.state
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "Startup transfer already started or cancelled"
        );
        Ok(())
    }

    /// Prevent queued work from starting after its caller stops waiting.
    pub fn close(&self) -> bool {
        self.state
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_err_and(|state| state == 1)
    }
}

pub fn ensure_not_cancelled(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Acquire) && !lianli_transport::usb::shutting_down(),
        "Startup image upload cancelled"
    );
    Ok(())
}

pub fn packet(
    builder: &mut crate::crypto::PacketBuilder,
    jpeg: &[u8],
    new_path: bool,
    fixed_size: bool,
    winusb: bool,
) -> Result<Vec<u8>> {
    let limit = if fixed_size { 101_888 } else { 1_048_576 };
    ensure!(
        !jpeg.is_empty() && jpeg.len() <= limit,
        "Startup JPEG exceeds device upload limit {limit}"
    );
    let mut packet = vec![
        0;
        if fixed_size {
            102_400
        } else {
            512 + jpeg.len()
        }
    ];
    packet[..512].copy_from_slice(&builder.startup_image_header(jpeg.len(), new_path, winusb));
    packet[512..512 + jpeg.len()].copy_from_slice(jpeg);
    Ok(packet)
}

pub fn revision(reply: &[u8]) -> Result<bool> {
    ensure!(
        reply.len() >= 10 && reply[0] == 0x80,
        "LCD revision reply missing or truncated; startup path is unknown"
    );
    Ok(reply[8] == 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_upload_cannot_start_late_and_started_upload_cannot_be_replayed() {
        let stop = AtomicBool::new(false);
        let cancelled = Transfer::default();
        assert!(!cancelled.close());
        assert!(cancelled.begin(&stop).is_err());
        assert!(!cancelled.close());
        let started = Transfer::default();
        started.begin(&stop).unwrap();
        assert!(started.close());
        assert!(started.begin(&stop).is_err());
        assert!(started.close());
    }

    #[test]
    fn fixed_upload_has_exact_limit_and_zero_padding() {
        let mut builder = crate::crypto::PacketBuilder::new();
        let data = packet(&mut builder, &[42; 3], false, true, false).unwrap();
        assert_eq!(data.len(), 102_400);
        assert_eq!(&data[512..515], &[42; 3]);
        assert!(data[515..].iter().all(|b| *b == 0));
        assert!(packet(&mut builder, &vec![1; 101_888], true, true, false).is_ok());
        assert!(packet(&mut builder, &vec![1; 101_889], true, true, false).is_err());
        assert!(packet(&mut builder, &[], false, false, true).is_err());
    }
    #[test]
    fn revision_requires_evidence_before_selecting_the_boot_path() {
        assert!(revision(&[0; 10]).is_err());
        let mut reply = [0; 10];
        reply[0] = 0x80;
        assert!(!revision(&reply).unwrap());
        reply[8] = 2;
        assert!(revision(&reply).unwrap());
        assert!(revision(&reply[..9]).is_err());
    }
}
