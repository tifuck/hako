//! hidraw node selection by report descriptor (sysfs only, no device I/O).
//!
//! Corsair devices expose several HID interfaces; the command channel is the
//! one whose descriptor declares a vendor usage page (0xFFC2 legacy, 0xFF42
//! Bragi, 0xFFC5 Void headsets). The output report length tells how large a
//! command packet must be (64, 128 or 1024 bytes depending on the model).

use crate::hid::{descriptor_items as items, enumerate_hidraw, HidDeviceInfo};
use anyhow::{Context, Result};
use std::path::Path;

/// Raw report descriptor of a hidraw node, read from sysfs.
pub fn report_descriptor(info: &HidDeviceInfo) -> Option<Vec<u8>> {
    let name = info.path.file_name()?;
    std::fs::read(
        Path::new("/sys/class/hidraw")
            .join(name)
            .join("device/report_descriptor"),
    )
    .ok()
}

/// Every Usage Page (global item 0x04) the descriptor declares.
pub fn usage_pages(desc: &[u8]) -> Vec<u16> {
    items(desc)
        .filter(|&(tag, _)| tag == 0x04)
        .map(|(_, v)| v as u16)
        .collect()
}

/// Output report sizes a command packet may have; anything else in a
/// descriptor is treated as absent (the caller's default applies).
const REPORT_LEN: std::ops::RangeInclusive<usize> = 8..=1024;

/// Payload bytes of output report `report_id` (0 = unnumbered), excluding
/// the id byte; `None` when absent or outside `REPORT_LEN`.
pub fn output_report_len(desc: &[u8], report_id: u8) -> Option<usize> {
    let (mut size, mut count, mut id, mut bits) = (0u32, 0u32, 0u32, 0u32);
    for (tag, v) in items(desc) {
        match tag {
            0x74 => size = v,
            0x94 => count = v,
            0x84 => id = v,
            0x90 if id == u32::from(report_id) => {
                bits = bits.saturating_add(size.saturating_mul(count))
            }
            _ => {}
        }
    }
    Some(bits.div_ceil(8) as usize).filter(|n| REPORT_LEN.contains(n))
}

/// The node of `vid:pid` on `iface` declaring `usage_page`; else any node
/// declaring it; else the one on `iface`.
pub fn find_node(vid: u16, pid: u16, usage_page: u16, iface: Option<u32>) -> Result<HidDeviceInfo> {
    let nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .collect();
    pick(&nodes, iface, |d| {
        report_descriptor(d).is_some_and(|desc| usage_pages(&desc).contains(&usage_page))
    })
    .cloned()
    .with_context(|| format!("no control interface for {vid:04x}:{pid:04x}"))
}

/// [`find_node`]'s choice among `nodes`; `declares`: the node has the usage
/// page (V2 receivers declare it on more than one interface).
fn pick(
    nodes: &[HidDeviceInfo],
    iface: Option<u32>,
    declares: impl Fn(&HidDeviceInfo) -> bool,
) -> Option<&HidDeviceInfo> {
    let on_iface = |d: &&HidDeviceInfo| iface.is_some() && d.interface == iface;
    nodes
        .iter()
        .find(|d| on_iface(d) && declares(d))
        .or_else(|| nodes.iter().find(|d| declares(d)))
        .or_else(|| nodes.iter().find(on_iface))
}

/// Stable instance key: the USB serial when the kernel reports one.
pub fn instance(info: &HidDeviceInfo) -> String {
    if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vendor collection as Corsair Bragi devices declare it: usage page
    /// 0xFF42, 64-byte unnumbered input and output reports.
    const BRAGI_DESC: &[u8] = &[
        0x06, 0x42, 0xff, // Usage Page (0xFF42)
        0x09, 0x01, // Usage (1)
        0xa1, 0x01, // Collection (Application)
        0x09, 0x01, 0x15, 0x00, 0x26, 0xff, 0x00, // Usage, Logical 0..255
        0x75, 0x08, 0x95, 0x40, // Report Size 8, Count 64
        0x81, 0x02, // Input
        0x09, 0x02, 0x75, 0x08, 0x95, 0x40, // Usage, Size 8, Count 64
        0x91, 0x02, // Output
        0xc0, // End Collection
    ];

    #[test]
    fn parses_usage_page_and_output_length() {
        assert_eq!(usage_pages(BRAGI_DESC), vec![0xff42]);
        assert_eq!(output_report_len(BRAGI_DESC, 0), Some(64));
        assert_eq!(output_report_len(BRAGI_DESC, 2), None);
    }

    #[test]
    fn numbered_reports_are_counted_per_id() {
        let desc = [
            0x06, 0x42, 0xff, 0xa1, 0x01, // page 0xFF42, collection
            0x85, 0x01, 0x75, 0x08, 0x95, 0x3f, 0x81, 0x02, // id 1: 63-byte input
            0x85, 0x02, 0x75, 0x08, 0x96, 0xff, 0x03, 0x91, 0x02, // id 2: 1023-byte output
            0xc0,
        ];
        assert_eq!(output_report_len(&desc, 2), Some(1023));
        assert_eq!(output_report_len(&desc, 1), None);
    }

    #[test]
    fn implausible_output_lengths_are_rejected() {
        let out = |size: u8, count: &[u8]| {
            let mut d = vec![0x06, 0x42, 0xff, 0xa1, 0x01, 0x75, size];
            d.push(if count.len() == 1 { 0x95 } else { 0x97 });
            d.extend_from_slice(count);
            d.extend_from_slice(&[0x91, 0x02, 0xc0]);
            output_report_len(&d, 0)
        };
        assert_eq!(out(8, &[3]), None);
        assert_eq!(out(8, &[8]), Some(8));
        assert_eq!(out(8, &[0x00, 0x04, 0x00, 0x00]), Some(1024));
        assert_eq!(out(8, &[0x01, 0x04, 0x00, 0x00]), None);
        // Size x count past u32: saturates, then is rejected.
        assert_eq!(out(0xff, &[0xff, 0xff, 0xff, 0xff]), None);
    }

    #[test]
    fn the_page_on_the_given_interface_wins() {
        let node = |iface| HidDeviceInfo {
            path: format!("/dev/hidraw{iface}").into(),
            vendor_id: 0x1b1c,
            product_id: 0x2a0c,
            interface: Some(iface),
            uniq: String::new(),
            bus: 3,
            name: String::new(),
            sys: Default::default(),
        };
        let nodes = [node(0), node(3), node(4)];
        let iface = |n: Option<&HidDeviceInfo>| n.and_then(|d| d.interface);
        // Interfaces 3 and 4 both declare the page: the asked one is taken.
        let vendor = |d: &HidDeviceInfo| d.interface != Some(0);
        assert_eq!(iface(pick(&nodes, Some(4), vendor)), Some(4));
        assert_eq!(iface(pick(&nodes, Some(3), vendor)), Some(3));
        // Not declared there: any node declaring it, then the interface alone.
        let only3 = |d: &HidDeviceInfo| d.interface == Some(3);
        assert_eq!(iface(pick(&nodes, Some(4), only3)), Some(3));
        assert_eq!(iface(pick(&nodes, Some(4), |_| false)), Some(4));
        assert_eq!(iface(pick(&nodes, None, |_| false)), None);
    }

    #[test]
    fn long_items_and_truncation_are_skipped() {
        assert_eq!(
            usage_pages(&[0xfe, 0x02, 0x10, 0xaa, 0xbb, 0x05, 0x0c]),
            vec![0x0c]
        );
        // A 2-byte item cut short ends the walk instead of panicking.
        assert!(usage_pages(&[0x06, 0x42]).is_empty());
    }
}
