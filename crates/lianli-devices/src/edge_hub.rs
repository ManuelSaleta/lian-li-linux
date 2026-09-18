use crate::registry::{DeviceDriver, OpenContext, OpenedDevice, SharedHid};
use crate::traits::SensorDevice;
use anyhow::{ensure, Context, Result};
use lianli_shared::device_id::{DeviceFamily, TransportKind};
use lianli_shared::ipc::{DeviceTelemetry, DeviceTemperature};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

fn request_packet(command: u8) -> Result<[u8; 65]> {
    ensure!(
        matches!(command, 1 | 2 | 5),
        "unsupported Edge Hub read request"
    );
    let mut packet = [0; 65];
    packet[0] = 1;
    packet[1] = command;
    Ok(packet)
}

fn response_payload(packet: &[u8], command: u8) -> Result<&[u8]> {
    ensure!(
        packet.len() >= 11 && packet[0] == 1 && packet[1] == command,
        "invalid Edge Hub response header"
    );
    let length = usize::from(u16::from_be_bytes([packet[9], packet[10]]));
    ensure!(length <= 54, "invalid Edge Hub response length");
    packet
        .get(11..11 + length)
        .context("truncated Edge Hub response")
}

fn read(backend: &SharedHid, command: u8) -> Result<Vec<u8>> {
    ensure!(
        !lianli_transport::usb::shutting_down(),
        "Edge Hub is stopping"
    );
    let mut transport = backend
        .try_lock_for(Duration::from_millis(100))
        .context("Edge Hub transport busy")?;
    let packet = request_packet(command)?;
    ensure!(
        transport.write_timeout(&packet, Duration::from_millis(500))? == packet.len(),
        "short Edge Hub request write"
    );
    ensure!(
        !lianli_transport::usb::shutting_down(),
        "Edge Hub is stopping"
    );
    let mut response = [0; 65];
    let length = transport.read_timeout(&mut response, 500)?;
    Ok(response_payload(&response[..length], command)?.to_vec())
}

fn temperatures(payload: &[u8]) -> Result<Vec<DeviceTemperature>> {
    ensure!(payload.len() >= 9, "truncated Edge Hub temperatures");
    Ok(["GPU 2 connector", "GPU 1 connector", "Hub ambient"]
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let fraction = payload[index * 2 + 1];
            // The vendor encodes the fractional byte as decimal digits, not fixed point.
            let scale = if fraction < 10 {
                10.0
            } else if fraction < 100 {
                100.0
            } else {
                1000.0
            };
            let abnormal = payload[6 + index] != 0;
            DeviceTemperature {
                name: (*name).into(),
                celsius: (!abnormal)
                    .then_some(f32::from(payload[index * 2]) + f32::from(fraction) / scale),
                abnormal,
            }
        })
        .collect())
}

#[derive(Default)]
struct Snapshot {
    data: DeviceTelemetry,
    observed: Option<Instant>,
}

impl Snapshot {
    fn telemetry(&self, now: Instant) -> DeviceTelemetry {
        let mut data = self.data.clone();
        data.age_ms = self.observed.map(|at| {
            now.saturating_duration_since(at)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        });
        if data.error.is_some() || data.age_ms.is_none_or(|age| age >= 2500) {
            for reading in &mut data.temperatures {
                reading.celsius = None;
            }
            if data.error.is_none() {
                data.error = Some("Temperature readings are stale".into());
            }
        }
        data
    }
}

pub struct EdgeHub {
    state: Arc<Mutex<Snapshot>>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl EdgeHub {
    fn open(backend: SharedHid) -> Result<Self> {
        let product = read(&backend, 2).context("reading Edge Hub product information")?;
        ensure!(product.len() >= 4, "truncated Edge Hub product information");
        let serial = read(&backend, 5).ok().and_then(|bytes| {
            let bytes = bytes.get(..12)?;
            let text = std::str::from_utf8(bytes)
                .ok()?
                .trim_matches(char::from(0))
                .trim();
            (!text.is_empty() && text.bytes().all(|b| b.is_ascii_graphic() || b == b' '))
                .then(|| text.to_owned())
        });
        let state = Arc::new(Mutex::new(Snapshot {
            data: DeviceTelemetry {
                serial,
                firmware: Some(format!("{}.{:02}", product[2], product[3])),
                product_type: Some(product[0]),
                product_subtype: Some(product[1]),
                temperatures: temperatures(&[0, 0, 0, 0, 0, 0, 1, 1, 1])?,
                error: Some("Waiting for temperature readings".into()),
                ..Default::default()
            },
            observed: None,
        }));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name("edge-hub".into())
            .spawn(move || {
                let mut failures = 0u32;
                loop {
                    if *worker_stop.0.lock() || lianli_transport::usb::shutting_down() {
                        break;
                    }
                    let result = read(&backend, 1).and_then(|payload| temperatures(&payload));
                    {
                        let mut snapshot = worker_state.lock();
                        match result {
                            Ok(readings) => {
                                snapshot.data.temperatures = readings;
                                snapshot.data.error = None;
                                snapshot.observed = Some(Instant::now());
                                failures = 0;
                            }
                            Err(error) => {
                                snapshot.data.error = Some(error.to_string());
                                failures = failures.saturating_add(1);
                            }
                        }
                    }
                    let mut stopped = worker_stop.0.lock();
                    if *stopped {
                        break;
                    }
                    worker_stop.1.wait_for(
                        &mut stopped,
                        Duration::from_secs(u64::from(failures.clamp(1, 5))),
                    );
                }
            })
            .context("starting Edge Hub telemetry worker")?;
        Ok(Self {
            state,
            stop,
            worker: Some(worker),
        })
    }
}

impl SensorDevice for EdgeHub {
    fn invalidate(&self) {
        let mut snapshot = self.state.lock();
        snapshot.observed = None;
        snapshot.data.error = Some("Waiting for a fresh reading after resume".into());
        self.stop.1.notify_all();
    }
    fn telemetry(&self) -> DeviceTelemetry {
        self.state.lock().telemetry(Instant::now())
    }
}

impl Drop for EdgeHub {
    fn drop(&mut self) {
        *self.stop.0.lock() = true;
        self.stop.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct EdgeHubDriver;

impl DeviceDriver for EdgeHubDriver {
    fn family(&self) -> DeviceFamily {
        DeviceFamily::EdgeHub
    }

    fn open(&self, ctx: &OpenContext) -> Result<OpenedDevice> {
        let backend = crate::detect::open_shared_hid(
            &ctx.device,
            ctx.hid_usage_page,
            ctx.vid,
            ctx.pid,
            ctx.bus,
            &ctx.device.port_numbers().unwrap_or_default(),
            ctx.hid_backend,
        )?;
        let hub = EdgeHub::open(backend.clone())?;
        Ok(OpenedDevice {
            id: ctx.device_id(),
            family: self.family(),
            capabilities: self.family().capabilities(),
            transport_kind: TransportKind::Hid,
            model_name: "Edge Hub Advanced".into(),
            firmware: hub.telemetry().firmware,
            sensors: Some(Box::new(hub)),
            fan: None,
            lcd: None,
            rgb: Vec::new(),
            aio: None,
            shared_hid: Some(backend),
            shared_usb: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_read_only_and_use_zero_length_envelopes() {
        for command in [1, 2, 5] {
            let packet = request_packet(command).unwrap();
            assert_eq!(&packet[..2], &[1, command]);
            assert!(packet[2..].iter().all(|b| *b == 0));
        }
        assert!(request_packet(3).is_err());
    }

    #[test]
    fn replies_require_matching_command_and_actual_payload_bytes() {
        let mut packet = request_packet(1).unwrap();
        packet[10] = 9;
        assert!(response_payload(&packet[..19], 1).is_err());
        assert_eq!(response_payload(&packet[..20], 1).unwrap().len(), 9);
        assert!(response_payload(&packet, 2).is_err());
        packet[10] = 55;
        assert!(response_payload(&packet, 1).is_err());
    }

    #[test]
    fn temperatures_preserve_decimal_digits_and_abnormal_state() {
        let values = temperatures(&[25, 12, 34, 5, 18, 125, 0, 1, 0]).unwrap();
        assert_eq!(values[0].celsius, Some(25.12));
        assert_eq!(values[1].celsius, None);
        assert!(values[1].abnormal);
        assert_eq!(values[2].celsius, Some(18.125));
        assert!(temperatures(&[0; 8]).is_err());
    }

    #[test]
    fn failed_or_stale_readings_never_publish_zero_as_a_valid_temperature() {
        let now = Instant::now();
        let mut state = Snapshot {
            observed: Some(now),
            data: DeviceTelemetry {
                temperatures: temperatures(&[25, 2, 26, 4, 27, 6, 0, 0, 0]).unwrap(),
                ..Default::default()
            },
        };
        assert_eq!(state.telemetry(now).temperatures[0].celsius, Some(25.2));
        assert!(state
            .telemetry(now + Duration::from_millis(2500))
            .temperatures
            .iter()
            .all(|t| t.celsius.is_none()));
        state.data.error = Some("read failed".into());
        assert!(state
            .telemetry(now)
            .temperatures
            .iter()
            .all(|t| t.celsius.is_none()));
    }
}
