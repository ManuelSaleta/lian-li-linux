use anyhow::{ensure, Context, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::session::{DesktopSession, SessionKind};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

type Bus = *mut c_void;
type Message = *mut c_void;
type Slot = *mut c_void;
type Handler = unsafe extern "C" fn(Message, *mut c_void, *mut c_void) -> c_int;
type ReadBasic = unsafe extern "C" fn(Message, c_char, *mut c_void) -> c_int;
type GetMember = unsafe extern "C" fn(Message) -> *const c_char;

macro_rules! bus_api {
    ($($name:ident: $ty:ty),+ $(,)?) => {
        struct Api {
            $($name: $ty,)+
            _library: libloading::Library,
        }

        impl Api {
            fn load() -> Result<Self> {
                // All copied symbols remain valid until the last owned bus/message is closed.
                unsafe {
                    let library = libloading::Library::new("libsystemd.so.0").context("Loading logind client library")?;
                    Ok(Self {
                        $($name: *library.get::<$ty>(concat!(stringify!($name), "\0").as_bytes())?,)+
                        _library: library,
                    })
                }
            }
        }
    };
}

bus_api! {
    sd_bus_new: unsafe extern "C" fn(*mut Bus) -> c_int,
    sd_bus_set_address: unsafe extern "C" fn(Bus, *const c_char) -> c_int,
    sd_bus_set_bus_client: unsafe extern "C" fn(Bus, c_int) -> c_int,
    sd_bus_set_method_call_timeout: unsafe extern "C" fn(Bus, u64) -> c_int,
    sd_bus_start: unsafe extern "C" fn(Bus) -> c_int,
    sd_bus_is_ready: unsafe extern "C" fn(Bus) -> c_int,
    sd_bus_close: unsafe extern "C" fn(Bus),
    sd_bus_unref: unsafe extern "C" fn(Bus) -> Bus,
    sd_bus_add_match: unsafe extern "C" fn(Bus, *mut Slot, *const c_char, Handler, *mut c_void) -> c_int,
    sd_bus_slot_unref: unsafe extern "C" fn(Slot) -> Slot,
    sd_bus_get_fd: unsafe extern "C" fn(Bus) -> c_int,
    sd_bus_get_events: unsafe extern "C" fn(Bus) -> c_int,
    sd_bus_get_timeout: unsafe extern "C" fn(Bus, *mut u64) -> c_int,
    sd_bus_process: unsafe extern "C" fn(Bus, *mut Message) -> c_int,
    sd_bus_get_property: unsafe extern "C" fn(Bus, *const c_char, *const c_char, *const c_char, *const c_char, *mut c_void, *mut Message, *const c_char) -> c_int,
    sd_bus_get_property_trivial: unsafe extern "C" fn(Bus, *const c_char, *const c_char, *const c_char, *const c_char, *mut c_void, c_char, *mut c_void) -> c_int,
    sd_bus_message_enter_container: unsafe extern "C" fn(Message, c_char, *const c_char) -> c_int,
    sd_bus_message_read_basic: unsafe extern "C" fn(Message, c_char, *mut c_void) -> c_int,
    sd_bus_message_get_member: GetMember,
    sd_bus_message_unref: unsafe extern "C" fn(Message) -> Message,
}

pub struct LoginMonitor {
    bus: Bus,
    slot: Slot,
    signals: Box<LoginSignals>,
    api: Api,
}

pub struct LoginEvents {
    pub changed: bool,
    pub pending: bool,
    pub session_ended: bool,
}

struct LoginSignals {
    dirty: AtomicBool,
    ended: AtomicBool,
    watched: Option<CString>,
    read_basic: ReadBasic,
    get_member: GetMember,
}

impl LoginMonitor {
    pub fn connect(context: &InstallationContext) -> Result<Self> {
        let address = match context {
            InstallationContext::Native => c"unix:path=/run/dbus/system_bus_socket",
            InstallationContext::Distrobox { .. } => {
                c"unix:path=/run/host/run/dbus/system_bus_socket"
            }
            InstallationContext::UnsupportedContainer => {
                anyhow::bail!("Host login sessions are not visible from this container")
            }
        };
        Self::connect_address(address)
    }

    fn connect_address(address: &CStr) -> Result<Self> {
        let api = Api::load()?;
        let mut bus = ptr::null_mut();
        status(
            unsafe { (api.sd_bus_new)(&mut bus) },
            "creating logind connection",
        )?;
        let mut monitor = Self {
            bus,
            slot: ptr::null_mut(),
            signals: Box::new(LoginSignals {
                dirty: AtomicBool::new(true),
                ended: AtomicBool::new(false),
                watched: None,
                read_basic: api.sd_bus_message_read_basic,
                get_member: api.sd_bus_message_get_member,
            }),
            api,
        };
        // The connection owns bus, and all strings remain alive through these synchronous calls.
        unsafe {
            status(
                (monitor.api.sd_bus_set_address)(bus, address.as_ptr()),
                "selecting host system bus",
            )?;
            status(
                (monitor.api.sd_bus_set_bus_client)(bus, 1),
                "configuring logind connection",
            )?;
            status(
                (monitor.api.sd_bus_set_method_call_timeout)(bus, 500_000),
                "bounding logind calls",
            )?;
            status(
                (monitor.api.sd_bus_start)(bus),
                "starting logind connection",
            )?;
        }
        monitor.wait_ready(Instant::now() + Duration::from_millis(500))?;
        unsafe {
            status(
                (monitor.api.sd_bus_add_match)(
                    bus,
                    &mut monitor.slot,
                    c"type='signal',sender='org.freedesktop.login1'".as_ptr(),
                    changed,
                    (&*monitor.signals as *const LoginSignals).cast_mut().cast(),
                ),
                "watching login-session changes",
            )?;
        }
        Ok(monitor)
    }

    fn wait_ready(&self, deadline: Instant) -> Result<()> {
        loop {
            let ready = unsafe { (self.api.sd_bus_is_ready)(self.bus) };
            status(ready, "checking system bus authentication")?;
            if ready > 0 {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "System bus authentication timed out");
            let processed = unsafe { (self.api.sd_bus_process)(self.bus, ptr::null_mut()) };
            status(processed, "authenticating system bus")?;
            if processed > 0 {
                continue;
            }
            let (fd, events) = self.poll_descriptor()?;
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events,
                revents: 0,
            };
            let result =
                unsafe { libc::poll(&mut poll, 1, remaining.as_millis().clamp(1, 50) as i32) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            }
        }
    }

    pub fn active_session(&self) -> Result<Option<DesktopSession>> {
        let (id, path) = self.seat_session()?;
        if id.is_empty() {
            return Ok(None);
        }
        let path = CString::new(path)?;
        let user = self.property(&path, c"org.freedesktop.login1.Session", c"User", c"(uo)")?;
        user.enter(c"uo")?;
        let uid = user.uid()?;
        let kind = self.string(&path, c"Type")?;
        let class = self.string(&path, c"Class")?;
        let active = self.boolean(&path, c"Active")?;
        let remote = self.boolean(&path, c"Remote")?;
        let locked = self.boolean(&path, c"LockedHint")?;
        let (current, _) = self.seat_session()?;
        if current != id {
            return Ok(None);
        }
        Ok(eligible_session(
            id, uid, &kind, &class, active, remote, locked,
        ))
    }

    pub fn watch_session(&mut self, id: &str) -> Result<()> {
        self.signals.watched = Some(CString::new(id)?);
        self.signals.ended.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn process(&self) -> Result<LoginEvents> {
        let mut processed = 0;
        while processed < 32 {
            let result = unsafe { (self.api.sd_bus_process)(self.bus, ptr::null_mut()) };
            status(result, "processing login-session events")?;
            if result == 0 {
                break;
            }
            processed += 1;
        }
        Ok(LoginEvents {
            changed: self.signals.dirty.swap(false, Ordering::Relaxed),
            pending: processed == 32,
            session_ended: self.signals.ended.load(Ordering::Relaxed),
        })
    }

    pub fn poll_descriptor(&self) -> Result<(BorrowedFd<'_>, i16)> {
        let fd: RawFd = unsafe { (self.api.sd_bus_get_fd)(self.bus) };
        status(fd, "obtaining login monitor descriptor")?;
        let events = unsafe { (self.api.sd_bus_get_events)(self.bus) };
        status(events, "obtaining login monitor events")?;
        // sd-bus owns this descriptor until the monitor is dropped.
        Ok((unsafe { BorrowedFd::borrow_raw(fd) }, events as i16))
    }

    pub fn timeout(&self) -> Result<Option<Duration>> {
        let mut deadline = u64::MAX;
        status(
            unsafe { (self.api.sd_bus_get_timeout)(self.bus, &mut deadline) },
            "obtaining login monitor timeout",
        )?;
        if deadline == u64::MAX {
            return Ok(None);
        }
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } == 0,
            "Reading monotonic clock failed"
        );
        let now = (now.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(now.tv_nsec as u64 / 1000);
        Ok(Some(Duration::from_micros(deadline.saturating_sub(now))))
    }

    fn seat_session(&self) -> Result<(String, String)> {
        let message = self.property(
            c"/org/freedesktop/login1/seat/seat0",
            c"org.freedesktop.login1.Seat",
            c"ActiveSession",
            c"(so)",
        )?;
        message.enter(c"so")?;
        Ok((message.string(b's')?, message.string(b'o')?))
    }

    fn property(
        &self,
        path: &CStr,
        interface: &CStr,
        property: &CStr,
        signature: &CStr,
    ) -> Result<BusMessage<'_>> {
        let mut message = ptr::null_mut();
        let result = unsafe {
            (self.api.sd_bus_get_property)(
                self.bus,
                c"org.freedesktop.login1".as_ptr(),
                path.as_ptr(),
                interface.as_ptr(),
                property.as_ptr(),
                ptr::null_mut(),
                &mut message,
                signature.as_ptr(),
            )
        };
        let message = BusMessage {
            raw: message,
            api: &self.api,
        };
        status(
            result,
            &format!("reading logind {}", property.to_string_lossy()),
        )?;
        ensure!(!message.raw.is_null(), "Logind returned no property value");
        Ok(message)
    }

    fn string(&self, path: &CStr, property: &CStr) -> Result<String> {
        self.property(path, c"org.freedesktop.login1.Session", property, c"s")?
            .string(b's')
    }

    fn boolean(&self, path: &CStr, property: &CStr) -> Result<bool> {
        let mut value: c_int = 0;
        let result = unsafe {
            (self.api.sd_bus_get_property_trivial)(
                self.bus,
                c"org.freedesktop.login1".as_ptr(),
                path.as_ptr(),
                c"org.freedesktop.login1.Session".as_ptr(),
                property.as_ptr(),
                ptr::null_mut(),
                b'b' as c_char,
                (&mut value as *mut c_int).cast(),
            )
        };
        status(
            result,
            &format!("reading logind {}", property.to_string_lossy()),
        )?;
        Ok(value != 0)
    }
}

impl Drop for LoginMonitor {
    fn drop(&mut self) {
        // Stop callbacks before releasing their userdata or unloading the library; never flush on shutdown.
        unsafe {
            (self.api.sd_bus_slot_unref)(self.slot);
            (self.api.sd_bus_close)(self.bus);
            (self.api.sd_bus_unref)(self.bus);
        }
    }
}

unsafe extern "C" fn changed(message: Message, userdata: *mut c_void, _: *mut c_void) -> c_int {
    // The monitor keeps this boxed flag alive until its match slot is released.
    let signals = unsafe { &*userdata.cast::<LoginSignals>() };
    signals.dirty.store(true, Ordering::Relaxed);
    if let Some(watched) = &signals.watched {
        let member = unsafe { (signals.get_member)(message) };
        if !member.is_null() && unsafe { CStr::from_ptr(member) } == c"SessionRemoved" {
            let mut id: *const c_char = ptr::null();
            let read = unsafe {
                (signals.read_basic)(
                    message,
                    b's' as c_char,
                    (&mut id as *mut *const c_char).cast(),
                )
            };
            if read > 0 && !id.is_null() && unsafe { CStr::from_ptr(id) } == watched.as_c_str() {
                signals.ended.store(true, Ordering::Relaxed);
            }
        }
    }
    0
}

struct BusMessage<'a> {
    raw: Message,
    api: &'a Api,
}

impl BusMessage<'_> {
    fn enter(&self, signature: &CStr) -> Result<()> {
        let result = unsafe {
            (self.api.sd_bus_message_enter_container)(self.raw, b'r' as c_char, signature.as_ptr())
        };
        status(result, "reading logind tuple")?;
        ensure!(result > 0, "Missing logind tuple");
        Ok(())
    }

    fn string(&self, kind: u8) -> Result<String> {
        let mut value: *const c_char = ptr::null();
        let result = unsafe {
            (self.api.sd_bus_message_read_basic)(
                self.raw,
                kind as c_char,
                (&mut value as *mut *const c_char).cast(),
            )
        };
        status(result, "reading logind string")?;
        ensure!(result > 0 && !value.is_null(), "Missing logind string");
        // The message retains the C string while it is checked and copied.
        let value = unsafe { CStr::from_ptr(value) };
        ensure!(
            value.to_bytes().len() <= 512,
            "Logind string exceeds 512 bytes"
        );
        Ok(value.to_str()?.to_owned())
    }

    fn uid(&self) -> Result<u32> {
        let mut value = 0u32;
        let result = unsafe {
            (self.api.sd_bus_message_read_basic)(
                self.raw,
                b'u' as c_char,
                (&mut value as *mut u32).cast(),
            )
        };
        status(result, "reading logind user ID")?;
        ensure!(result > 0, "Missing logind user ID");
        Ok(value)
    }
}

impl Drop for BusMessage<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.api.sd_bus_message_unref)(self.raw);
        }
    }
}

fn status(value: c_int, operation: &str) -> Result<()> {
    if value < 0 {
        return Err(io::Error::from_raw_os_error(-value)).context(operation.to_owned());
    }
    Ok(())
}

fn eligible_session(
    id: String,
    uid: u32,
    kind: &str,
    class: &str,
    active: bool,
    remote: bool,
    locked: bool,
) -> Option<DesktopSession> {
    if !active
        || remote
        || !matches!(
            class,
            "user" | "user-early" | "user-light" | "user-early-light"
        )
    {
        return None;
    }
    let kind = match kind {
        "wayland" => SessionKind::Wayland,
        "x11" => SessionKind::X11,
        _ => return None,
    };
    Some(DesktopSession {
        id,
        uid,
        kind,
        locked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_remote_inactive_greeter_and_text_sessions() {
        assert!(
            eligible_session("2".into(), 1000, "wayland", "user", true, false, false).is_some()
        );
        for (kind, class, active, remote) in [
            ("wayland", "greeter", true, false),
            ("tty", "user", true, false),
            ("x11", "user", false, false),
            ("wayland", "user", true, true),
        ] {
            assert!(
                eligible_session("2".into(), 1000, kind, class, active, remote, false).is_none()
            );
        }
        let session =
            eligible_session("2".into(), 1000, "wayland", "user", true, false, true).unwrap();
        assert!(session.locked);
        assert!(!session.allows_capture(1000, "2"));
    }

    #[test]
    fn unresponsive_bus_cannot_hold_session_authorization_indefinitely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let address = CString::new(format!("unix:path={}", path.display())).unwrap();
        let started = std::time::Instant::now();
        assert!(LoginMonitor::connect_address(&address).is_err());
        assert!(started.elapsed() < Duration::from_millis(1500));
        worker.join().unwrap();
    }
}
