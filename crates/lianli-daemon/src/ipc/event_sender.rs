use crate::service::DaemonEvent;
use lianli_control::write_gate::ServiceWritePermit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

#[derive(Clone)]
pub(crate) struct EventSender {
    sender: mpsc::Sender<DaemonEvent>,
    permit: Option<Arc<ServiceWritePermit>>,
    delivery_failed: Option<Arc<AtomicBool>>,
}

impl EventSender {
    pub fn new(sender: mpsc::Sender<DaemonEvent>, permit: Option<ServiceWritePermit>) -> Self {
        let delivery_failed = permit.as_ref().map(|_| Arc::new(AtomicBool::new(false)));
        Self {
            sender,
            permit: permit.map(Arc::new),
            delivery_failed,
        }
    }

    pub fn send(&self, event: DaemonEvent) -> Result<(), mpsc::SendError<DaemonEvent>> {
        let event = match &self.permit {
            Some(permit) => DaemonEvent::Coordinated {
                event: Box::new(event),
                permit: permit.clone(),
            },
            None => event,
        };
        let result = self.sender.send(event);
        if result.is_err() {
            if let Some(failed) = &self.delivery_failed {
                failed.store(true, Ordering::Relaxed);
            }
        }
        result
    }

    pub fn delivery_failed(&self) -> bool {
        self.delivery_failed
            .as_ref()
            .is_some_and(|failed| failed.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
impl From<mpsc::Sender<DaemonEvent>> for EventSender {
    fn from(sender: mpsc::Sender<DaemonEvent>) -> Self {
        Self::new(sender, None)
    }
}
