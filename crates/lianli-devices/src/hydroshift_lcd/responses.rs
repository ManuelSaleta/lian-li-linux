use super::protocol::{A_HEADER_LEN, A_PACKET_SIZE};
use anyhow::{ensure, Result};
use std::time::{Duration, Instant};

pub(super) struct ResponseReader {
    deadline: Instant,
    remaining_reports: usize,
}

impl ResponseReader {
    pub(super) fn new(timeout_ms: i32) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(timeout_ms.max(1) as u64),
            remaining_reports: 64,
        }
    }

    pub(super) fn read(
        &mut self,
        mut read: impl FnMut(&mut [u8], i32) -> Result<usize>,
    ) -> Result<Vec<u8>> {
        loop {
            ensure!(
                !lianli_transport::usb::shutting_down(),
                "AIO response read cancelled"
            );
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "AIO response deadline expired");
            ensure!(
                self.remaining_reports > 0,
                "AIO response report limit exceeded"
            );
            let mut bytes = [0; A_PACKET_SIZE];
            let n = read(&mut bytes, remaining.as_millis().clamp(1, 100) as i32)?;
            if n == 0 {
                continue;
            }
            self.remaining_reports -= 1;
            ensure!(
                (2..=bytes.len()).contains(&n),
                "short or oversized AIO response"
            );
            return Ok(bytes[..n].to_vec());
        }
    }
}

pub(super) fn firmware_text(response: &[u8]) -> Result<String> {
    ensure!(
        response.len() >= A_HEADER_LEN,
        "short firmware response header"
    );
    let length = usize::from(response[5]);
    let data = response
        .get(A_HEADER_LEN..A_HEADER_LEN + length)
        .ok_or_else(|| anyhow::anyhow!("truncated firmware response payload"))?;
    Ok(String::from_utf8_lossy(data)
        .trim_end_matches('\0')
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_reports_cannot_extend_the_transaction_indefinitely() {
        let mut reader = ResponseReader::new(3000);
        let mut calls = 0;
        loop {
            let result = reader.read(|bytes, timeout| {
                assert!((1..=100).contains(&timeout));
                calls += 1;
                bytes[..2].copy_from_slice(&[1, 0x81]);
                Ok(2)
            });
            if result.is_err() {
                break;
            }
        }
        assert_eq!(calls, 64);
    }

    #[test]
    fn expired_deadline_does_not_start_another_read() {
        let mut reader = ResponseReader {
            deadline: Instant::now(),
            remaining_reports: 64,
        };
        assert!(reader.read(|_, _| panic!("read after deadline")).is_err());
    }

    #[test]
    fn firmware_payload_uses_actual_report_length() {
        assert!(firmware_text(&[1, 0x86]).is_err());
        assert!(firmware_text(&[1, 0x86, 0, 0, 0, 3, b'1']).is_err());
        assert_eq!(
            firmware_text(&[1, 0x86, 0, 0, 0, 4, b'1', b'.', b'7', 0]).unwrap(),
            "1.7"
        );
    }

    #[test]
    fn one_byte_reply_cannot_inherit_previous_opcode() {
        let mut reader = ResponseReader::new(3000);
        assert!(reader
            .read(|bytes, _| {
                bytes[0] = 1;
                Ok(1)
            })
            .is_err());
    }
}
