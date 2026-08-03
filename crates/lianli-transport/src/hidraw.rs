use crate::error::TransportError;
use crate::hid_trait::HidTransport;
use hidapi::HidDevice;

pub struct HidrawTransport {
    device: HidDevice,
}

impl HidrawTransport {
    pub fn new(device: HidDevice) -> Self {
        Self { device }
    }
}

impl HidTransport for HidrawTransport {
    fn write(&mut self, data: &[u8]) -> Result<usize, TransportError> {
        self.device
            .write(data)
            .map_err(|e| TransportError::Write(e.to_string()))
    }

    fn read_timeout(&mut self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, TransportError> {
        self.device
            .read_timeout(buf, timeout_ms)
            .map_err(|e| TransportError::Read(e.to_string()))
    }

    fn send_feature_report(&mut self, data: &[u8]) -> Result<usize, TransportError> {
        self.device
            .send_feature_report(data)
            .map_err(|e| TransportError::Write(e.to_string()))?;
        Ok(data.len())
    }

    fn get_feature_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.device
            .get_feature_report(buf)
            .map_err(|e| TransportError::Read(e.to_string()))
    }

    fn get_input_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.device
            .get_input_report(buf)
            .map_err(|e| TransportError::Read(e.to_string()))
    }

    fn read_flush(&mut self) {
        let mut buf = [0u8; 64];
        loop {
            match self.device.read_timeout(&mut buf, 5) {
                Ok(n) if n > 0 => continue,
                _ => break,
            }
        }
    }
}
