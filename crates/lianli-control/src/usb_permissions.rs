use anyhow::{ensure, Context, Result};
use lianli_shared::config::HidBackend;
use lianli_shared::device_id::{lookup_device, UsbId, V2_HID_COMPANION};
use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationContext, InstallationFinding, InstallationGuide,
};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 2048;
const MAX_DEVICES: usize = 64;

struct Device {
    sysfs: PathBuf,
    label: String,
    hid: bool,
    usb_node: PathBuf,
    number: u64,
}

struct Node {
    device: PathBuf,
    path: PathBuf,
    number: u64,
}

fn attribute(path: &Path) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("Reading {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "Not a sysfs attribute");
    let mut value = String::new();
    file.take(129).read_to_string(&mut value)?;
    ensure!(
        value.len() <= 128,
        "Sysfs attribute exceeds the check limit"
    );
    Ok(value.trim().into())
}

fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("Listing {}", path.display()))? {
        ensure!(
            entries.len() < MAX_ENTRIES,
            "Sysfs inventory exceeds the check limit"
        );
        entries.push(entry?.path());
    }
    entries.sort();
    Ok(entries)
}

fn number(path: &Path) -> Result<u64> {
    let value = attribute(path)?;
    let (major, minor) = value
        .split_once(':')
        .context("Invalid sysfs device number")?;
    Ok(libc::makedev(major.parse()?, minor.parse()?))
}

fn devices(root: &Path) -> Result<Vec<Device>> {
    let mut devices = Vec::new();
    let tree = root.join("sys/devices").canonicalize()?;
    for path in entries(&root.join("sys/bus/usb/devices"))? {
        if path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().contains(&b':'))
        {
            continue;
        }
        let sysfs = path.canonicalize()?;
        ensure!(
            sysfs.starts_with(&tree),
            "USB sysfs entry is outside the device tree"
        );
        let vid = u16::from_str_radix(&attribute(&sysfs.join("idVendor"))?, 16)?;
        let pid = u16::from_str_radix(&attribute(&sysfs.join("idProduct"))?, 16)?;
        let Some((name, hid)) = transport(vid, pid) else {
            continue;
        };
        ensure!(
            devices.len() < MAX_DEVICES,
            "Supported USB inventory exceeds the check limit"
        );
        let bus: u8 = attribute(&sysfs.join("busnum"))?.parse()?;
        let address: u8 = attribute(&sysfs.join("devnum"))?.parse()?;
        ensure!(bus != 0 && address != 0, "Invalid USB bus/address");
        devices.push(Device {
            label: format!("{name} {vid:04x}:{pid:04x} at {bus}:{address}"),
            hid,
            usb_node: root.join(format!("dev/bus/usb/{bus:03}/{address:03}")),
            number: number(&sysfs.join("dev"))?,
            sysfs,
        });
    }
    Ok(devices)
}

fn transport(vid: u16, pid: u16) -> Option<(&'static str, bool)> {
    if UsbId::new(vid, pid) == V2_HID_COMPANION {
        return Some(("V2 dongle HID companion", true));
    }
    lookup_device(vid, pid).map(|entry| (entry.name, entry.family.uses_hid()))
}

fn hid_nodes(root: &Path) -> Result<Vec<Node>> {
    let tree = root.join("sys/devices").canonicalize()?;
    let mut nodes = Vec::new();
    for path in entries(&root.join("sys/class/hidraw"))? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invalid HID node name")?;
        ensure!(
            name.strip_prefix("hidraw").is_some_and(
                |suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
            ),
            "Invalid HID node name"
        );
        let device = path.join("device").canonicalize()?;
        ensure!(
            device.starts_with(&tree),
            "HID sysfs entry is outside the device tree"
        );
        nodes.push(Node {
            device,
            path: root.join("dev").join(name),
            number: number(&path.join("dev"))?,
        });
    }
    Ok(nodes)
}

fn node_access(path: &Path, number: u64) -> Result<bool> {
    // O_PATH pins metadata without invoking the USB/HID driver's open operation.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("Inspecting {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.file_type().is_char_device() && metadata.rdev() == number,
        "{} is not the character device reported by sysfs",
        path.display()
    );
    crate::runtime_health::effective_access(&file)
}

fn device_access(
    device: &Device,
    backend: HidBackend,
    nodes: &Result<Vec<Node>>,
    check: &mut impl FnMut(&Path, u64) -> Result<bool>,
) -> Result<bool> {
    if !device.hid || backend == HidBackend::Rusb {
        return check(&device.usb_node, device.number);
    }
    let nodes = nodes
        .as_ref()
        .map_err(|error| anyhow::anyhow!("HID inventory unavailable: {error:#}"))?;
    let candidates: Vec<_> = nodes
        .iter()
        .filter(|node| node.device.starts_with(&device.sysfs))
        .collect();
    ensure!(!candidates.is_empty(), "No hidraw node is visible at this USB topology. Check kernel binding and container device visibility");
    let mut allowed = 0;
    for node in &candidates {
        allowed += usize::from(check(&node.path, node.number)?);
    }
    ensure!(allowed == 0 || allowed == candidates.len(), "HID interfaces have different permissions. The required usage-page interface cannot be established without further driver evidence");
    Ok(allowed != 0)
}

fn inspect_at(
    root: &Path,
    backend: HidBackend,
    mut check: impl FnMut(&Path, u64) -> Result<bool>,
) -> Result<(CheckState, String)> {
    let devices = devices(root)?;
    if devices.is_empty() {
        return Ok((CheckState::NotApplicable, "No supported USB devices are visible in this process's sysfs namespace. This is not evidence of permission denial or complete host visibility.".into()));
    }
    let nodes = if backend == HidBackend::Hidraw && devices.iter().any(|device| device.hid) {
        hid_nodes(root)
    } else {
        Ok(Vec::new())
    };
    let (mut allowed, mut denied, mut unavailable) = (0, 0, 0);
    let mut details = Vec::new();
    for device in &devices {
        let detail = match device_access(device, backend, &nodes, &mut check) {
            Ok(true) => {
                allowed += 1;
                None
            }
            Ok(false) => {
                denied += 1;
                Some(format!("{}: permission denied", device.label))
            }
            Err(error) => {
                unavailable += 1;
                Some(format!("{}: {error:#}", device.label))
            }
        };
        if let Some(detail) = detail.filter(|_| details.len() < 8) {
            details.push(detail.chars().take(400).collect::<String>());
        }
    }
    let state = if denied > 0 {
        CheckState::Failed
    } else if unavailable > 0 {
        CheckState::Unavailable
    } else {
        CheckState::Passed
    };
    Ok((state, format!("HID backend {backend}. {} supported devices: {allowed} accessible, {denied} denied, {unavailable} unverified. {} Node checks include ACLs. Device initialization and security-policy access were not tested.", devices.len(), details.join("\n"))))
}

pub fn inspect(context: &InstallationContext, backend: HidBackend) -> InstallationFinding {
    let result = if matches!(context, InstallationContext::UnsupportedContainer) {
        Err(anyhow::anyhow!(
            "Host/device visibility is unavailable in this unsupported container"
        ))
    } else {
        inspect_at(Path::new("/"), backend, node_access)
    };
    let (state, evidence) = result.unwrap_or_else(|error| {
        (
            CheckState::Unavailable,
            format!("USB permission checks unavailable: {error:#}"),
        )
    });
    InstallationFinding {
        code: "usb.node_access".into(), state,
        severity: match state { CheckState::Passed | CheckState::NotApplicable => FindingSeverity::Info, CheckState::Failed => FindingSeverity::Error, CheckState::Unavailable => FindingSeverity::Warning },
        feature: "USB access".into(), context: format!("Effective UID {}, HID backend {backend}", unsafe { libc::geteuid() }),
        title: "USB/HID node permissions".into(), evidence,
        remediation: "Apply host USB rules and group changes, then restart the login or service. For Distrobox, stop and re-enter the box. Missing hidraw nodes need kernel or device-visibility repair. Check daemon logs for open failures.".into(),
        guide: if matches!(context, InstallationContext::Native) { InstallationGuide::UsbPermissions } else { InstallationGuide::Distrobox },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for path in ["sys/devices", "sys/bus/usb/devices", "sys/class/hidraw"] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        root
    }

    fn usb(root: &Path, topology: &str, id: UsbId, address: u8) -> PathBuf {
        let device = root.join("sys/devices").join(topology);
        fs::create_dir_all(&device).unwrap();
        for (name, value) in [
            ("idVendor", format!("{:04x}", id.vid)),
            ("idProduct", format!("{:04x}", id.pid)),
            ("busnum", "1".into()),
            ("devnum", address.to_string()),
            ("dev", format!("189:{}", address - 1)),
        ] {
            fs::write(device.join(name), value).unwrap();
        }
        symlink(&device, root.join("sys/bus/usb/devices").join(topology)).unwrap();
        device
    }

    fn hid(root: &Path, device: &Path, index: u32) {
        let interface = device.join(format!("interface{index}/hid"));
        fs::create_dir_all(&interface).unwrap();
        let node = root.join(format!("sys/class/hidraw/hidraw{index}"));
        fs::create_dir_all(&node).unwrap();
        symlink(interface, node.join("device")).unwrap();
        fs::write(node.join("dev"), format!("240:{index}")).unwrap();
    }

    #[test]
    fn checks_the_selected_transport_and_exact_usb_topology() {
        let root = fixture();
        let first = usb(root.path(), "1-1", V2_HID_COMPANION, 1);
        let second = usb(root.path(), "1-10", V2_HID_COMPANION, 2);
        hid(root.path(), &first, 0);
        hid(root.path(), &second, 1);
        let mut seen = Vec::new();
        let result = inspect_at(root.path(), HidBackend::Hidraw, |path, number| {
            seen.push((path.strip_prefix(root.path()).unwrap().to_owned(), number));
            Ok(!path.ends_with("hidraw1"))
        })
        .unwrap();
        assert_eq!(result.0, CheckState::Failed);
        assert!(result.1.contains("1 accessible, 1 denied"));
        assert!(result.1.contains("at 1:2: permission denied"));
        assert_eq!(
            seen,
            vec![
                ("dev/hidraw0".into(), libc::makedev(240, 0)),
                ("dev/hidraw1".into(), libc::makedev(240, 1))
            ]
        );
        seen.clear();
        let result = inspect_at(root.path(), HidBackend::Rusb, |path, number| {
            seen.push((path.strip_prefix(root.path()).unwrap().to_owned(), number));
            Ok(true)
        })
        .unwrap();
        assert_eq!(result.0, CheckState::Passed);
        assert_eq!(
            seen,
            vec![
                ("dev/bus/usb/001/001".into(), libc::makedev(189, 0)),
                ("dev/bus/usb/001/002".into(), libc::makedev(189, 1))
            ]
        );
    }

    #[test]
    fn missing_and_ambiguous_hid_nodes_are_not_permission_denials() {
        let root = fixture();
        let device = usb(root.path(), "1-1", V2_HID_COMPANION, 1);
        let missing = inspect_at(root.path(), HidBackend::Hidraw, |_, _| {
            panic!("No node to check")
        })
        .unwrap();
        assert_eq!(missing.0, CheckState::Unavailable);
        assert!(missing.1.contains("No hidraw node"));
        hid(root.path(), &device, 0);
        hid(root.path(), &device, 1);
        let mixed = inspect_at(root.path(), HidBackend::Hidraw, |path, _| {
            Ok(path.ends_with("hidraw0"))
        })
        .unwrap();
        assert_eq!(mixed.0, CheckState::Unavailable);
        assert!(mixed.1.contains("different permissions"));
        assert_eq!(
            inspect_at(root.path(), HidBackend::Hidraw, |_, _| Ok(false))
                .unwrap()
                .0,
            CheckState::Failed
        );
        assert_eq!(
            inspect_at(root.path(), HidBackend::Hidraw, |_, _| Ok(true))
                .unwrap()
                .0,
            CheckState::Passed
        );
        assert_eq!(
            inspect_at(root.path(), HidBackend::Hidraw, |_, _| Err(
                anyhow::anyhow!("Node disappeared")
            ))
            .unwrap()
            .0,
            CheckState::Unavailable
        );
    }

    #[test]
    fn absent_unsupported_and_bulk_devices_do_not_require_hidraw() {
        let root = fixture();
        let absent = inspect_at(root.path(), HidBackend::Hidraw, |_, _| {
            panic!("No device I/O")
        })
        .unwrap();
        assert_eq!(absent.0, CheckState::NotApplicable);
        usb(root.path(), "1-1", UsbId::new(0xffff, 0xffff), 1);
        assert_eq!(
            inspect_at(root.path(), HidBackend::Hidraw, |_, _| panic!(
                "Unsupported device"
            ))
            .unwrap()
            .0,
            CheckState::NotApplicable
        );
        let bulk = lianli_shared::device_id::KNOWN_DEVICES
            .iter()
            .find(|entry| entry.family.uses_usb_bulk())
            .unwrap();
        usb(root.path(), "1-2", bulk.id, 2);
        fs::remove_dir(root.path().join("sys/class/hidraw")).unwrap();
        let result = inspect_at(root.path(), HidBackend::Hidraw, |path, number| {
            assert!(path.ends_with("dev/bus/usb/001/002"));
            assert_eq!(number, libc::makedev(189, 1));
            Ok(true)
        })
        .unwrap();
        assert_eq!(result.0, CheckState::Passed);
        fs::remove_dir_all(root.path().join("sys/bus/usb/devices")).unwrap();
        assert!(inspect_at(root.path(), HidBackend::Hidraw, |_, _| panic!(
            "Hidden sysfs"
        ))
        .is_err());
    }

    #[test]
    fn inventory_limits_and_replaced_nodes_remain_unverified() {
        let root = fixture();
        let device = usb(root.path(), "1-1", V2_HID_COMPANION, 1);
        fs::write(device.join("idVendor"), "a".repeat(129)).unwrap();
        assert!(devices(root.path()).is_err());
        let path = root.path().join("fake-device");
        fs::write(&path, b"retained").unwrap();
        assert!(node_access(&path, libc::makedev(189, 0)).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"retained");
        let alias = root.path().join("alias");
        symlink(&path, &alias).unwrap();
        assert!(node_access(&alias, libc::makedev(189, 0)).is_err());
        let missing = root.path().join("missing");
        assert!(node_access(&missing, 0).is_err());
        assert!(!missing.exists());
        let too_many = root.path().join("large");
        fs::create_dir(&too_many).unwrap();
        for index in 0..=MAX_ENTRIES {
            fs::write(too_many.join(index.to_string()), b"").unwrap();
        }
        assert!(entries(&too_many).is_err());
    }
}
