use crate::error::TransportError;

pub trait HidTransport: Send {
    fn write(&mut self, data: &[u8]) -> Result<usize, TransportError>;
    fn read_timeout(&mut self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, TransportError>;
    fn send_feature_report(&mut self, data: &[u8]) -> Result<usize, TransportError>;
    fn get_feature_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
    fn get_input_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
    fn read_flush(&mut self);
}

impl<T: HidTransport + ?Sized> HidTransport for Box<T> {
    fn write(&mut self, data: &[u8]) -> Result<usize, TransportError> {
        (**self).write(data)
    }
    fn read_timeout(&mut self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, TransportError> {
        (**self).read_timeout(buf, timeout_ms)
    }
    fn send_feature_report(&mut self, data: &[u8]) -> Result<usize, TransportError> {
        (**self).send_feature_report(data)
    }
    fn get_feature_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        (**self).get_feature_report(buf)
    }
    fn get_input_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        (**self).get_input_report(buf)
    }
    fn read_flush(&mut self) {
        (**self).read_flush()
    }
}
