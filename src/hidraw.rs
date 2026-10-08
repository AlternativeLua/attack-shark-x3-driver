//! Minimal Linux hidraw access: sysfs discovery, a HID report-descriptor
//! scan, and the handful of syscalls needed to exchange reports.
//!
//! hidraw sits beside the kernel's own HID driver, so the mouse keeps working
//! as a mouse while it is being configured; nothing is detached or claimed.

use std::ffi::{c_int, c_ulong};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(not(target_os = "linux"))]
compile_error!("x3ctl talks to Linux hidraw and only builds for Linux");

// The _IOC() bit layout below is the asm-generic one.
#[cfg(any(
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "sparc",
    target_arch = "sparc64",
))]
compile_error!("ioctl request numbers are encoded differently on this architecture");

const IOC_WRITE: c_ulong = 1;
const IOC_READ: c_ulong = 2;

/// `_IOC(dir, 'H', nr, size)` from `<asm-generic/ioctl.h>`.
const fn ioc(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
    dir << 30 | (size as c_ulong) << 16 | (b'H' as c_ulong) << 8 | nr
}

// From <linux/hidraw.h>.
const HIDIOCGRAWINFO: c_ulong = ioc(IOC_READ, 0x03, size_of::<RawInfo>());
const fn hidiocsfeature(len: usize) -> c_ulong {
    ioc(IOC_READ | IOC_WRITE, 0x06, len)
}
const fn hidiocgfeature(len: usize) -> c_ulong {
    ioc(IOC_READ | IOC_WRITE, 0x07, len)
}

/// `struct hidraw_devinfo`
#[repr(C)]
#[derive(Default)]
struct RawInfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}

/// `struct pollfd`
#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 0x0001;

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
}

/// One `/dev/hidrawN` node, described from sysfs without opening it.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub node: PathBuf,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    /// USB interface number, when the node hangs off a USB interface.
    pub interface: Option<u8>,
    /// `(report id, payload bytes)` of every feature report it declares.
    pub features: Vec<(u8, usize)>,
}

impl DeviceInfo {
    pub fn feature_len(&self, report: u8) -> Option<usize> {
        self.features.iter().find(|f| f.0 == report).map(|f| f.1)
    }
}

/// Lists the hidraw nodes belonging to `vendor`, in node order.
pub fn scan(vendor: u16) -> io::Result<Vec<DeviceInfo>> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/sys/class/hidraw")? {
        let entry = entry?;
        let sys = entry.path().join("device");
        // Nodes can vanish mid-scan; skip whatever cannot be read.
        let Ok(uevent) = fs::read_to_string(sys.join("uevent")) else {
            continue;
        };
        let Some((vid, pid)) = uevent_value(&uevent, "HID_ID").and_then(parse_hid_id) else {
            continue;
        };
        if vid != vendor {
            continue;
        }
        let Ok(descriptor) = fs::read(sys.join("report_descriptor")) else {
            continue;
        };
        // .../<usb interface>/<hid device>: the interface is the parent.
        let interface = fs::canonicalize(&sys)
            .ok()
            .and_then(|hid| fs::read_to_string(hid.parent()?.join("bInterfaceNumber")).ok())
            .and_then(|n| u8::from_str_radix(n.trim(), 16).ok());
        found.push(DeviceInfo {
            node: PathBuf::from("/dev").join(entry.file_name()),
            name: uevent_value(&uevent, "HID_NAME").unwrap_or("?").to_string(),
            vendor: vid,
            product: pid,
            interface,
            features: feature_reports(&descriptor),
        });
    }
    found.sort_by_key(|d| node_number(&d.node));
    Ok(found)
}

fn node_number(node: &std::path::Path) -> u32 {
    node.to_string_lossy()
        .trim_start_matches("/dev/hidraw")
        .parse()
        .unwrap_or(u32::MAX)
}

fn uevent_value<'a>(uevent: &'a str, key: &str) -> Option<&'a str> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

/// `HID_ID=0003:00001D57:0000FA61` is bus:vendor:product in hex.
fn parse_hid_id(id: &str) -> Option<(u16, u16)> {
    let mut fields = id.split(':').skip(1).map(|f| u32::from_str_radix(f, 16));
    let vendor = fields.next()?.ok()?;
    let product = fields.next()?.ok()?;
    Some((vendor as u16, product as u16))
}

/// Walks a HID report descriptor and returns `(report id, payload bytes)`
/// for each feature report, in id order.
pub fn feature_reports(descriptor: &[u8]) -> Vec<(u8, usize)> {
    // Global item state; Push/Pop save and restore it.
    #[derive(Clone, Copy, Default)]
    struct Globals {
        report_id: u8,
        size_bits: usize,
        count: usize,
    }
    let mut globals = Globals::default();
    let mut stack = Vec::new();
    let mut bits = [0usize; 256];

    let mut rest = descriptor;
    while let Some((&prefix, tail)) = rest.split_first() {
        if prefix == 0xfe {
            // Long item: data length, long tag, data.
            let Some(&len) = tail.first() else { break };
            rest = tail.get(2 + usize::from(len)..).unwrap_or(&[]);
            continue;
        }
        let len = [0, 1, 2, 4][usize::from(prefix & 0x03)];
        let Some(data) = tail.get(..len) else { break };
        rest = &tail[len..];
        let value = data
            .iter()
            .rev()
            .fold(0usize, |acc, &b| acc << 8 | usize::from(b));

        // Upper six bits are tag (4) and type (2): 0 = main, 1 = global.
        match prefix & 0xfc {
            0x74 => globals.size_bits = value,
            0x84 => globals.report_id = value as u8,
            0x94 => globals.count = value,
            0xa4 => stack.push(globals),
            0xb4 => globals = stack.pop().unwrap_or(globals),
            0xb0 => {
                let report = &mut bits[usize::from(globals.report_id)];
                *report = report.saturating_add(globals.size_bits.saturating_mul(globals.count));
            }
            _ => {}
        }
    }

    (0u8..=255)
        .zip(bits)
        .filter(|&(_, bits)| bits != 0)
        .map(|(id, bits)| (id, bits.div_ceil(8)))
        .collect()
}

/// An open hidraw node.
pub struct Hidraw {
    file: File,
}

impl Hidraw {
    pub fn open(info: &DeviceInfo) -> io::Result<Hidraw> {
        let file = OpenOptions::new().read(true).write(true).open(&info.node)?;

        // `info` came from sysfs before the node was opened; if the device was
        // replugged in between, hidrawN may now be something else entirely.
        let mut raw = RawInfo::default();
        // SAFETY: HIDIOCGRAWINFO writes one hidraw_devinfo through the pointer.
        if unsafe { ioctl(file.as_raw_fd(), HIDIOCGRAWINFO, &mut raw as *mut RawInfo) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if (raw.vendor as u16, raw.product as u16) != (info.vendor, info.product) {
            return Err(io::Error::other(format!(
                "is now {:04x}:{:04x}, not the device that was found; try again",
                raw.vendor as u16, raw.product as u16
            )));
        }
        Ok(Hidraw { file })
    }

    /// Sends a feature report (HID SET_REPORT). `report[0]` is the report id.
    pub fn set_feature(&self, report: &[u8]) -> io::Result<()> {
        // SAFETY: HIDIOCSFEATURE(len) reads `len` bytes from the pointer.
        let n = unsafe {
            ioctl(
                self.file.as_raw_fd(),
                hidiocsfeature(report.len()),
                report.as_ptr(),
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Fetches a feature report (HID GET_REPORT) of at most `len` bytes,
    /// report id included.
    pub fn get_feature(&self, report: u8, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len.max(1)];
        buf[0] = report;
        // SAFETY: HIDIOCGFEATURE(len) reads the id from, then writes at most
        // `len` bytes to, the pointer.
        let n = unsafe {
            ioctl(
                self.file.as_raw_fd(),
                hidiocgfeature(buf.len()),
                buf.as_mut_ptr(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }

    /// Waits up to `timeout` for the next input report. `None` on timeout.
    pub fn read_input(&self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        let mut pfd = PollFd {
            fd: self.file.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        let millis = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
        loop {
            // SAFETY: `pfd` is one valid pollfd for the duration of the call.
            match unsafe { poll(&mut pfd, 1, millis) } {
                0 => return Ok(None),
                n if n > 0 => break,
                _ => {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::Interrupted {
                        return Err(err);
                    }
                }
            }
        }
        // On POLLERR/POLLHUP the read below reports what happened.
        let mut buf = [0u8; 64];
        let n = (&self.file).read(&mut buf)?;
        Ok(Some(buf[..n].to_vec()))
    }

    /// Discards input reports that are already queued.
    pub fn drain(&self) -> io::Result<()> {
        while self.read_input(Duration::ZERO)?.is_some() {}
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(HIDIOCGRAWINFO, 0x8008_4803);
        assert_eq!(hidiocsfeature(9), 0xc009_4806);
        assert_eq!(hidiocgfeature(56), 0xc038_4807);
    }

    #[test]
    fn hid_id() {
        let uevent = "DRIVER=hid-generic\nHID_ID=0003:00001D57:0000FA61\nHID_NAME=Beken X3 Mouse\n";
        assert_eq!(uevent_value(uevent, "HID_NAME"), Some("Beken X3 Mouse"));
        assert_eq!(
            uevent_value(uevent, "HID_ID").and_then(parse_hid_id),
            Some((0x1d57, 0xfa61))
        );
        assert_eq!(parse_hid_id("0003:zz:1"), None);
    }

    /// Interface 2 of a wired X3, as read from sysfs.
    const X3_CONFIG_DESCRIPTOR: &[u8] = &[
        0x05, 0x01, 0x09, 0x80, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x01, 0x19, 0x81, 0x29, 0x83, 0x15,
        0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x06, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
        0xc0, 0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x02, 0x15, 0x00, 0x26, 0x80, 0x03, 0x19,
        0x00, 0x2a, 0x80, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xc0, 0x05, 0x0a, 0x09, 0x00,
        0xa1, 0x01, 0x85, 0x03, 0x19, 0x00, 0x29, 0x00, 0x15, 0x00, 0x25, 0x00, 0x75, 0x08, 0x95,
        0x04, 0x81, 0x01, 0xc0, 0x05, 0x0b, 0x09, 0x00, 0xa1, 0x01, 0x85, 0x04, 0x95, 0x33, 0xb1,
        0x01, 0x85, 0x05, 0x95, 0x0c, 0xb1, 0x01, 0x85, 0x06, 0x95, 0x08, 0xb1, 0x01, 0x85, 0x07,
        0x95, 0x07, 0xb1, 0x01, 0x85, 0x08, 0x95, 0x3a, 0xb1, 0x01, 0x85, 0x09, 0x95, 0x3f, 0xb1,
        0x01, 0x85, 0x0a, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x0b, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x0c,
        0x95, 0x05, 0xb1, 0x01, 0x85, 0x10, 0x95, 0x07, 0xb1, 0x01, 0x85, 0xa0, 0x95, 0x07, 0xb1,
        0x01, 0x85, 0x22, 0x95, 0x83, 0xb1, 0x01, 0x85, 0x23, 0x95, 0x83, 0xb1, 0x01, 0x85, 0x24,
        0x95, 0x07, 0xb1, 0x01, 0x85, 0x25, 0x95, 0x0c, 0xb1, 0x01, 0x85, 0x26, 0x95, 0x08, 0xb1,
        0x01, 0x85, 0x27, 0x95, 0x07, 0xb1, 0x01, 0x85, 0x28, 0x95, 0x83, 0xb1, 0x01, 0x85, 0x29,
        0x95, 0x80, 0xb1, 0x01, 0x85, 0x2a, 0x95, 0x82, 0xb1, 0x01, 0x85, 0x2b, 0x95, 0x07, 0xb1,
        0x01, 0x85, 0x2c, 0x95, 0x03, 0xb1, 0x01, 0x85, 0x2d, 0x95, 0x66, 0xb1, 0x01, 0x85, 0x23,
        0x95, 0x82, 0xb1, 0x01, 0x85, 0x2e, 0x95, 0x04, 0xb1, 0x01, 0xc0,
    ];

    #[test]
    fn feature_reports_of_the_x3_config_interface() {
        let features = feature_reports(X3_CONFIG_DESCRIPTOR);
        let len = |id| features.iter().find(|f| f.0 == id).map(|f| f.1);
        assert_eq!(len(0x04), Some(51));
        assert_eq!(len(0x05), Some(12));
        assert_eq!(len(0x06), Some(8));
        assert_eq!(len(0xa0), Some(7));
        // Reports 1..3 are input-only and must not show up.
        assert_eq!(len(0x01), None);
        assert_eq!(len(0x03), None);
        assert_eq!(features.len(), 24);
    }

    #[test]
    fn feature_reports_survives_truncated_and_long_items() {
        assert_eq!(feature_reports(&[]), []);
        // Item claims 2 data bytes but only 1 follows.
        assert_eq!(feature_reports(&[0x85, 0x04, 0x76, 0x08]), []);
        // A long item (0xfe, 3 data bytes) before a 1-byte feature report 9.
        let d = [
            0xfe, 0x03, 0x00, 1, 2, 3, 0x85, 0x09, 0x75, 0x08, 0x95, 0x01, 0xb1, 0x01,
        ];
        assert_eq!(feature_reports(&d), [(9, 1)]);
        // Push/Pop restores the report id.
        let d = [
            0x85, 0x05, 0x75, 0x08, 0x95, 0x02, 0xa4, 0x85, 0x06, 0xb4, 0xb1, 0x01,
        ];
        assert_eq!(feature_reports(&d), [(5, 2)]);
    }
}
