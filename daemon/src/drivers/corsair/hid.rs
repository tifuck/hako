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

/// Payload bytes of output report `report_id` (0 = unnumbered), excluding the id byte.
pub fn output_report_len(desc: &[u8], report_id: u8) -> Option<usize> {
    let (mut size, mut count, mut id, mut bits) = (0u32, 0u32, 0u32, 0u32);
    for (tag, v) in items(desc) {
        match tag {
            0x74 => size = v,
            0x94 => count = v,
            0x84 => id = v,
            0x90 if id == u32::from(report_id) => bits += size * count,
            _ => {}
        }
    }
    (bits > 0).then(|| bits.div_ceil(8) as usize)
}

/// The node of `vid:pid` declaring `usage_page`; else the one on `iface`.
pub fn find_node(vid: u16, pid: u16, usage_page: u16, iface: Option<u32>) -> Result<HidDeviceInfo> {
    let nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .collect();
    nodes
        .iter()
        .find(|d| report_descriptor(d).is_some_and(|desc| usage_pages(&desc).contains(&usage_page)))
        .or_else(|| iface.and_then(|i| nodes.iter().find(|d| d.interface == Some(i))))
        .cloned()
        .with_context(|| format!("no control interface for {vid:04x}:{pid:04x}"))
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
    fn long_items_and_truncation_are_skipped() {
        assert_eq!(
            usage_pages(&[0xfe, 0x02, 0x10, 0xaa, 0xbb, 0x05, 0x0c]),
            vec![0x0c]
        );
        // A 2-byte item cut short ends the walk instead of panicking.
        assert!(usage_pages(&[0x06, 0x42]).is_empty());
    }
}
