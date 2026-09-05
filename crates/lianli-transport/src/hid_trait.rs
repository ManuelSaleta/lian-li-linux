use crate::error::TransportError;

pub trait HidTransport: Send {
    fn write(&mut self, data: &[u8]) -> Result<usize, TransportError>;
    fn read_timeout(&mut self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, TransportError>;
    fn send_feature_report(&mut self, data: &[u8]) -> Result<usize, TransportError>;
    fn get_feature_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
    fn get_input_report(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
    fn read_flush(&mut self);
    fn reopen_count(&self) -> u64 {
        0
    }
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
    fn reopen_count(&self) -> u64 {
        (**self).reopen_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTransport {
        count: u64,
    }

    impl HidTransport for MockTransport {
        fn write(&mut self, data: &[u8]) -> Result<usize, TransportError> {
            Ok(data.len())
        }
        fn read_timeout(&mut self, _buf: &mut [u8], _timeout_ms: i32) -> Result<usize, TransportError> {
            Ok(0)
        }
        fn send_feature_report(&mut self, data: &[u8]) -> Result<usize, TransportError> {
            Ok(data.len())
        }
        fn get_feature_report(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
            Ok(0)
        }
        fn get_input_report(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
            Ok(0)
        }
        fn read_flush(&mut self) {}
        fn reopen_count(&self) -> u64 {
            self.count
        }
    }

    struct DefaultTransport;
    impl HidTransport for DefaultTransport {
        fn write(&mut self, data: &[u8]) -> Result<usize, TransportError> {
            Ok(data.len())
        }
        fn read_timeout(&mut self, _buf: &mut [u8], _timeout_ms: i32) -> Result<usize, TransportError> {
            Ok(0)
        }
        fn send_feature_report(&mut self, data: &[u8]) -> Result<usize, TransportError> {
            Ok(data.len())
        }
        fn get_feature_report(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
            Ok(0)
        }
        fn get_input_report(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
            Ok(0)
        }
        fn read_flush(&mut self) {}
    }

    #[test]
    fn default_reopen_count_is_zero() {
        let t = DefaultTransport;
        assert_eq!(t.reopen_count(), 0);
        let boxed: Box<dyn HidTransport> = Box::new(t);
        assert_eq!(boxed.reopen_count(), 0);
    }

    #[test]
    fn custom_reopen_count_delegates_through_box() {
        let t = MockTransport { count: 42 };
        assert_eq!(t.reopen_count(), 42);
        let boxed: Box<dyn HidTransport> = Box::new(t);
        assert_eq!(boxed.reopen_count(), 42);
    }
}
