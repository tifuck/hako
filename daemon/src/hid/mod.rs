//! Pure-Rust hidraw helpers (no libusb-dev required).

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

const O_NONBLOCK: i32 = libc::O_NONBLOCK;

#[derive(Debug, Clone)]
pub struct HidDeviceInfo {
    pub path: PathBuf,
    pub vendor_id: u16,
    pub product_id: u16,
    pub interface: Option<u32>,
    pub uniq: String,
    /// `HID_ID` bus: [`BUS_USB`], [`BUS_BLUETOOTH`], 0x18 = I2C, ...
    pub bus: u16,
    /// `HID_NAME` ("Logitech USB Receiver").
    pub name: String,
    /// Canonical sysfs path of the HID device. Changes on every
    /// (re-)enumeration, so it tells a replugged device from the old one.
    pub sys: PathBuf,
}

/// Enumerate hidraw devices via sysfs.
pub fn enumerate_hidraw() -> Result<Vec<HidDeviceInfo>> {
    let mut out = Vec::new();
    let root = Path::new("/sys/class/hidraw");
    if !root.exists() {
        return Ok(out);
    }
    for ent in std::fs::read_dir(root)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let dev = ent.path().join("device");
        let uevent = std::fs::read_to_string(dev.join("uevent")).unwrap_or_default();
        let mut vid = 0u16;
        let mut pid = 0u16;
        let mut bus = 0u16;
        let mut uniq = String::new();
        let mut hid_name = String::new();
        for line in uevent.lines() {
            if let Some(rest) = line.strip_prefix("HID_ID=") {
                let parts: Vec<_> = rest.split(':').collect();
                if parts.len() >= 3 {
                    bus = u16::from_str_radix(parts[0], 16).unwrap_or(0);
                    vid = u16::from_str_radix(parts[1], 16).unwrap_or(0);
                    pid = u16::from_str_radix(parts[2], 16).unwrap_or(0);
                }
            }
            if let Some(rest) = line.strip_prefix("HID_UNIQ=") {
                uniq = rest.to_string();
            }
            if let Some(rest) = line.strip_prefix("HID_NAME=") {
                hid_name = rest.to_string();
            }
        }
        let interface = usb_interface(bus, &dev);
        out.push(HidDeviceInfo {
            path: PathBuf::from(format!("/dev/{name}")),
            vendor_id: vid,
            product_id: pid,
            interface,
            uniq,
            bus,
            name: hid_name,
            sys: std::fs::canonicalize(&dev).unwrap_or(dev),
        });
    }
    Ok(out)
}

pub const BUS_USB: u16 = 0x03;
pub const BUS_BLUETOOTH: u16 = 0x05;

impl HidDeviceInfo {
    /// The node's HID report descriptor (sysfs, no device I/O); empty if unreadable.
    pub fn report_descriptor(&self) -> Vec<u8> {
        std::fs::read(self.sys.join("report_descriptor")).unwrap_or_default()
    }
}

/// USB interface number of a HID device on `bus`. Only USB has one: walking
/// up from a Bluetooth device would reach its adapter's USB interface.
fn usb_interface(bus: u16, device: &Path) -> Option<u32> {
    if bus != BUS_USB {
        return None;
    }
    read_b_interface_number(device)
}

fn read_b_interface_number(device: &Path) -> Option<u32> {
    // hidrawN/device → HID device; parent is usually the USB interface (…:1.N).
    let mut p = device.to_path_buf();
    // Resolve symlink so parents are real USB nodes.
    if let Ok(canon) = std::fs::canonicalize(&p) {
        p = canon;
    }
    for _ in 0..8 {
        let f = p.join("bInterfaceNumber");
        if f.exists() {
            if let Ok(s) = std::fs::read_to_string(&f) {
                // Sysfs prints it as two hex digits ("%02x").
                return u32::from_str_radix(s.trim(), 16).ok();
            }
        }
        if !p.pop() {
            break;
        }
    }
    None
}

pub fn find_hidraw(vid: u16, pid: u16, interface: Option<u32>) -> Result<HidDeviceInfo> {
    let list = enumerate_hidraw()?;
    list.into_iter()
        .find(|d| {
            d.vendor_id == vid
                && d.product_id == pid
                && interface.map(|i| d.interface == Some(i)).unwrap_or(true)
        })
        .context("hidraw device not found")
}

pub struct HidrawDevice {
    file: File,
    pub path: PathBuf,
}

/// `_IOC(_IOC_READ|_IOC_WRITE, 'H', nr, len)`: HIDIOC\[SG\]FEATURE (6/7),
/// HIDIOCGINPUT (0x0a).
pub fn hidioc(nr: u64, len: usize) -> libc::c_ulong {
    (3u64 << 30) | ((len as u64) << 16) | (u64::from(b'H') << 8) | nr
}

/// Short items of a HID report descriptor as (tag|type byte without the size
/// bits, value). Long items are skipped; a truncated item ends the walk.
pub fn descriptor_items(desc: &[u8]) -> impl Iterator<Item = (u8, u32)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < desc.len() {
            let prefix = desc[i];
            if prefix == 0xfe {
                // Long item: 0xFE, data size, long tag, data.
                i += 3 + usize::from(*desc.get(i + 1)?);
                continue;
            }
            let size = [0, 1, 2, 4][usize::from(prefix & 3)];
            let data = desc.get(i + 1..i + 1 + size)?;
            i += 1 + size;
            let value = data
                .iter()
                .rev()
                .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
            return Some((prefix & 0xfc, value));
        }
        None
    })
}

/// Usage pages a HID report descriptor declares (global item `05`/`06 xx`).
pub fn usage_pages(desc: &[u8]) -> Vec<u16> {
    descriptor_items(desc)
        .filter(|&(tag, _)| tag == 0x04)
        .map(|(_, v)| v as u16)
        .collect()
}

/// Report IDs a HID report descriptor declares (global item `85 xx`).
pub fn report_ids(desc: &[u8]) -> Vec<u8> {
    descriptor_items(desc)
        .filter(|&(tag, _)| tag == 0x84)
        .map(|(_, v)| v as u8)
        .collect()
}

impl HidrawDevice {
    /// HIDIOCSFEATURE. `buf[0]` is the report id (0 if the device has none).
    pub fn set_feature(&self, buf: &mut [u8]) -> Result<()> {
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hidioc(0x06, buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if ret < 0 {
            bail!("HIDIOCSFEATURE: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// HIDIOCGFEATURE. Set `buf[0]` to the report id; data comes back in `buf[1..]`.
    pub fn get_feature(&self, buf: &mut [u8]) -> Result<()> {
        self.get_feature_len(buf).map(|_| ())
    }

    /// [`Self::get_feature`] that returns how many bytes (report id included)
    /// the device sent; the rest of `buf` is left as it was.
    pub fn get_feature_len(&self, buf: &mut [u8]) -> Result<usize> {
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hidioc(0x07, buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if ret < 0 {
            bail!("HIDIOCGFEATURE: {}", std::io::Error::last_os_error());
        }
        Ok(ret as usize)
    }

    pub fn open(path: &Path) -> Result<Self> {
        // Non-blocking so reads cannot hang the daemon forever.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "open {} failed: {e} (Permission denied usually means the hako udev rules are not installed, or this user has no active seat session)",
                    path.display()
                )
            })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// One report per call: hidraw sends every `write` as a report of its
    /// own, so a short write is an error, never completed by a second write.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        let start = Instant::now();
        loop {
            match self.file.write(data) {
                Ok(n) if n == data.len() => {
                    debug!("hid write {} bytes to {}", data.len(), self.path.display());
                    return Ok(());
                }
                Ok(n) => bail!(
                    "short hid write to {}: {n} of {} bytes",
                    self.path.display(),
                    data.len()
                ),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        && start.elapsed() < Duration::from_millis(200) =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Throw away queued input reports (input traffic, replies meant for
    /// other clients: hidraw hands every report to every open handle),
    /// waiting up to `wait` for each.
    pub fn drain(&mut self, wait: Duration) {
        let mut buf = [0u8; 1024];
        for _ in 0..64 {
            if self.read_timeout(&mut buf, wait).is_err() {
                break;
            }
        }
    }

    pub fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let start = Instant::now();
        let fd = self.file.as_raw_fd();
        loop {
            match self.file.read(buf) {
                Ok(n) if n > 0 => return Ok(n),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                bail!("hid read timeout on {}", self.path.display());
            }
            let wait_ms = remaining.as_millis().min(50) as i32;
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut pfd, 1, wait_ms);
            }
        }
    }
}

/// SteelSeries report padding to 64 bytes (common for GameDAC).
pub fn pad64(cmd: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 64];
    let n = cmd.len().min(64);
    v[..n].copy_from_slice(&cmd[..n]);
    v
}

// ── USB control transfers (OpenRazer-style SET_REPORT / GET_REPORT) ─────────

/// Locate `/dev/bus/usb/BBB/DDD` for a VID:PID.
pub fn find_usb_device_path(vid: u16, pid: u16) -> Result<PathBuf> {
    let root = Path::new("/sys/bus/usb/devices");
    if !root.exists() {
        bail!("no /sys/bus/usb/devices");
    }
    for ent in std::fs::read_dir(root)? {
        let ent = ent?;
        let p = ent.path();
        let v = std::fs::read_to_string(p.join("idVendor")).unwrap_or_default();
        let d = std::fs::read_to_string(p.join("idProduct")).unwrap_or_default();
        let Ok(vv) = u16::from_str_radix(v.trim(), 16) else {
            continue;
        };
        let Ok(pp) = u16::from_str_radix(d.trim(), 16) else {
            continue;
        };
        if vv == vid && pp == pid {
            let bus: u32 = std::fs::read_to_string(p.join("busnum"))?
                .trim()
                .parse()
                .context("busnum")?;
            let dev: u32 = std::fs::read_to_string(p.join("devnum"))?
                .trim()
                .parse()
                .context("devnum")?;
            return Ok(PathBuf::from(format!("/dev/bus/usb/{bus:03}/{dev:03}")));
        }
    }
    bail!("USB device {vid:04x}:{pid:04x} not found");
}

#[repr(C)]
struct UsbdevfsCtrltransfer {
    b_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    w_length: u16,
    timeout: u32,
    data: *mut libc::c_void,
}

fn usbdevfs_control_ioctl() -> libc::c_ulong {
    let size = std::mem::size_of::<UsbdevfsCtrltransfer>() as u64;
    // _IOWR('U', 0, struct usbdevfs_ctrltransfer) — nr field is 0
    (3u64 << 30) | (size << 16) | (u64::from(b'U') << 8)
}

const USBDEVFS_MAXDRIVERNAME: usize = 255;

#[repr(C)]
struct UsbdevfsDisconnectClaim {
    interface: libc::c_uint,
    flags: libc::c_uint,
    driver: [libc::c_char; USBDEVFS_MAXDRIVERNAME + 1],
}

#[repr(C)]
struct UsbdevfsIoctl {
    ifno: libc::c_int,
    ioctl_code: libc::c_int,
    data: *mut libc::c_void,
}

// _IOR('U', 27, struct usbdevfs_disconnect_claim)
fn usbdevfs_disconnect_claim_ioctl() -> libc::c_ulong {
    let size = std::mem::size_of::<UsbdevfsDisconnectClaim>() as u64;
    (2u64 << 30) | (size << 16) | (u64::from(b'U') << 8) | 27
}

// _IOR('U', 16, unsigned int)
fn usbdevfs_releaseinterface_ioctl() -> libc::c_ulong {
    let size = std::mem::size_of::<libc::c_uint>() as u64;
    (2u64 << 30) | (size << 16) | (u64::from(b'U') << 8) | 16
}

// _IOWR('U', 18, struct usbdevfs_ioctl) with embedded USBDEVFS_CONNECT (_IO('U', 23)).
fn usbdevfs_ioctl_ioctl() -> libc::c_ulong {
    let size = std::mem::size_of::<UsbdevfsIoctl>() as u64;
    (3u64 << 30) | (size << 16) | (u64::from(b'U') << 8) | 18
}
fn usbdevfs_connect_code() -> libc::c_int {
    // _IO('U', 23)
    ((u32::from(b'U') << 8) | 23) as libc::c_int
}

/// Open a USB device node for class control transfers (needs udev write access).
pub struct UsbDevice {
    file: File,
    pub path: PathBuf,
    /// Interface claimed from the kernel driver, released + reconnected on drop.
    claimed: Option<u16>,
}

impl UsbDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "open {} failed: {e} (install udev USB rule for this VID:PID; hidraw alone is not enough for Razer control transfers)",
                    path.display()
                )
            })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            claimed: None,
        })
    }

    /// Detach whatever kernel driver owns `interface` (usbhid) and claim it, so
    /// SET_REPORT/GET_REPORT to that interface no longer return `EBUSY`.
    pub fn claim_interface(&mut self, interface: u16) -> Result<()> {
        let mut dc = UsbdevfsDisconnectClaim {
            interface: interface as libc::c_uint,
            flags: 0, // unconditionally disconnect the current driver and claim
            driver: [0; USBDEVFS_MAXDRIVERNAME + 1],
        };
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                usbdevfs_disconnect_claim_ioctl(),
                &mut dc as *mut _,
            )
        };
        if ret < 0 {
            bail!(
                "USB claim iface={interface}: {}",
                std::io::Error::last_os_error()
            );
        }
        self.claimed = Some(interface);
        Ok(())
    }

    /// HID SET_REPORT (Feature, report id 0) on interface `w_index`.
    pub fn hid_set_feature(&self, interface: u16, data: &mut [u8]) -> Result<()> {
        let mut ctrl = UsbdevfsCtrltransfer {
            b_request_type: 0x21, // CLASS | INTERFACE | OUT
            b_request: 0x09,      // SET_REPORT
            w_value: 0x0300,      // Feature report, id 0
            w_index: interface,
            w_length: data.len() as u16,
            timeout: 1000,
            data: data.as_mut_ptr() as *mut libc::c_void,
        };
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                usbdevfs_control_ioctl(),
                &mut ctrl as *mut _,
            )
        };
        if ret < 0 {
            bail!(
                "USB SET_REPORT iface={interface}: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    /// HID GET_REPORT (Feature, report id 0) on interface `w_index`. A reply
    /// shorter than `data` is an error (OpenRazer `razer_get_usb_response`
    /// rejects any length but the full report too).
    pub fn hid_get_feature(&self, interface: u16, data: &mut [u8]) -> Result<()> {
        let mut ctrl = UsbdevfsCtrltransfer {
            b_request_type: 0xa1, // CLASS | INTERFACE | IN
            b_request: 0x01,      // GET_REPORT
            w_value: 0x0300,
            w_index: interface,
            w_length: data.len() as u16,
            timeout: 1000,
            data: data.as_mut_ptr() as *mut libc::c_void,
        };
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                usbdevfs_control_ioctl(),
                &mut ctrl as *mut _,
            )
        };
        if ret < 0 {
            bail!(
                "USB GET_REPORT iface={interface}: {}",
                std::io::Error::last_os_error()
            );
        }
        if (ret as usize) < data.len() {
            bail!(
                "USB GET_REPORT iface={interface}: short reply ({ret} of {} bytes)",
                data.len()
            );
        }
        Ok(())
    }
}

impl Drop for UsbDevice {
    fn drop(&mut self) {
        // Give the interface back to the kernel HID driver so the keyboard
        // returns to normal once the daemon stops.
        if let Some(iface) = self.claimed {
            let fd = self.file.as_raw_fd();
            let mut ifno: libc::c_uint = iface as libc::c_uint;
            let ret =
                unsafe { libc::ioctl(fd, usbdevfs_releaseinterface_ioctl(), &mut ifno as *mut _) };
            if ret < 0 {
                warn!(
                    "{}: release USB iface={iface}: {}",
                    self.path.display(),
                    std::io::Error::last_os_error()
                );
            }
            let mut reconnect = UsbdevfsIoctl {
                ifno: iface as libc::c_int,
                ioctl_code: usbdevfs_connect_code(),
                data: std::ptr::null_mut(),
            };
            let ret = unsafe { libc::ioctl(fd, usbdevfs_ioctl_ioctl(), &mut reconnect as *mut _) };
            if ret < 0 {
                warn!(
                    "{}: give USB iface={iface} back to the kernel driver: {}",
                    self.path.display(),
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_usb_devices_have_an_interface() {
        // usb/1-5:1.2/<hid device>, as sysfs nests a HID device in its interface.
        let root = std::env::temp_dir().join(format!("hako-iface-{}", std::process::id()));
        let hid = root.join("1-5:1.2/0005:046D:B023.0007");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::write(root.join("1-5:1.2/bInterfaceNumber"), "02\n").unwrap();
        assert_eq!(usb_interface(BUS_USB, &hid), Some(2));
        assert_eq!(usb_interface(BUS_BLUETOOTH, &hid), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn report_descriptor_ids() {
        // Vendor page 0xff00, collection, report id 0x10, ..., report id 0x11.
        let desc = [
            0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x10, 0x95, 0x06, 0x75, 0x08, 0x81,
            0x00, 0x85, 0x11, 0x95, 0x13, 0xc0,
        ];
        assert_eq!(report_ids(&desc), vec![0x10, 0x11]);
        // Boot keyboard without report ids.
        assert!(report_ids(&[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0xc0]).is_empty());
        // Data bytes that look like a Report ID prefix are skipped.
        assert!(report_ids(&[0x06, 0x85, 0x11]).is_empty());
        // A long item (size 2) hiding a fake `85 99`.
        let desc = [0x85, 0x21, 0xfe, 0x02, 0x10, 0x85, 0x99, 0x85, 0x66];
        assert_eq!(report_ids(&desc), vec![0x21, 0x66]);
    }
}
