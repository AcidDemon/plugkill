//! EDID base block parsing.
//!
//! The bytes come from whatever is plugged into the port, so this is untrusted
//! input handled by a daemon running as root. Every field is read at a fixed
//! offset through a bounds checked accessor, nothing the device sends sizes an
//! allocation, and any malformed input returns `None`. A panic here would stall
//! the poll loop of the process that is supposed to be watching the machine.

/// Length of the EDID base block. Extension blocks are not parsed.
pub const EDID_BASE_LEN: usize = 128;

/// Every EDID base block starts with this fixed pattern.
const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// Tag of the monitor name descriptor.
const TAG_MONITOR_NAME: u8 = 0xFC;

/// Longest monitor name kept. The descriptor holds 13 bytes, so this only
/// guards against a future change widening it.
const MAX_NAME: usize = 32;

/// A monitor's identity as reported by its EDID.
#[derive(Debug, Clone, Eq)]
pub struct EdidId {
    /// Three letter PNP manufacturer code, for example "SAM".
    pub mfg: String,
    /// Manufacturer assigned product code.
    pub product: u16,
    /// Unit serial. `None` when the monitor reports 0, which many panels do.
    pub serial: Option<u32>,
    /// Model name from the monitor name descriptor. May be empty.
    pub name: String,
}

/// Identity is manufacturer, product and serial. The name is a free text
/// descriptor that some monitors pad or truncate differently between reads, so
/// comparing it would produce violations that are not device changes. Spec B4.
impl PartialEq for EdidId {
    fn eq(&self, other: &Self) -> bool {
        self.mfg == other.mfg && self.product == other.product && self.serial == other.serial
    }
}

impl std::hash::Hash for EdidId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.mfg.hash(state);
        self.product.hash(state);
        self.serial.hash(state);
    }
}

impl EdidId {
    /// Stable identity string for selectors and log lines. A monitor with no
    /// serial identifies only to model level, the same way a USB device without
    /// a serial identifies only by vendor and product.
    pub fn selector(&self) -> String {
        match self.serial {
            Some(s) => format!("{}:{:04x}:{}", self.mfg, self.product, s),
            None => format!("{}:{:04x}", self.mfg, self.product),
        }
    }
}

/// Parse an EDID base block. Returns `None` for anything that is not one.
pub fn parse(bytes: &[u8]) -> Option<EdidId> {
    let b: &[u8; EDID_BASE_LEN] = bytes.get(..EDID_BASE_LEN)?.try_into().ok()?;
    if b[..8] != HEADER {
        return None;
    }

    // Three 5 bit letters packed big endian, 1 = 'A'. Anything outside that
    // range means this is not a manufacturer code, so the whole block is
    // rejected rather than guessed at.
    let packed = u16::from_be_bytes([b[8], b[9]]);
    let mut mfg = String::with_capacity(3);
    for shift in [10u16, 5, 0] {
        let v = ((packed >> shift) & 0x1F) as u8;
        if !(1..=26).contains(&v) {
            return None;
        }
        mfg.push((b'A' + v - 1) as char);
    }

    let product = u16::from_le_bytes([b[10], b[11]]);
    let serial = match u32::from_le_bytes([b[12], b[13], b[14], b[15]]) {
        0 => None,
        s => Some(s),
    };

    Some(EdidId {
        mfg,
        product,
        serial,
        name: monitor_name(b),
    })
}

/// Pull the monitor name out of the four 18 byte descriptors. A descriptor is a
/// monitor descriptor when it starts with three zero bytes; byte 3 is its tag.
fn monitor_name(b: &[u8; EDID_BASE_LEN]) -> String {
    for off in (54..126).step_by(18) {
        let Some(d) = b.get(off..off + 18) else {
            break;
        };
        if d[0..3] != [0, 0, 0] || d[3] != TAG_MONITOR_NAME {
            continue;
        }
        let text = &d[5..18];
        let end = text.iter().position(|&c| c == 0x0A).unwrap_or(text.len());
        let mut name: String = text[..end]
            .iter()
            .map(|&c| {
                if (0x20..0x7F).contains(&c) {
                    c as char
                } else {
                    ' '
                }
            })
            .collect();
        let trimmed = name.trim().to_string();
        name = trimmed;
        name.truncate(MAX_NAME);
        return name;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real EDID from a Samsung Odyssey G93SD, which reports a unique serial.
    const SAMSUNG: &str = "00ffffffffffff004c2d2d77313657302a230104b57722783be095ae4e3abb25\
0c505421080081c0810081809500a9c0b3000101010191e600a0f03896403020\
3a00a9504100001a000000fd0c30f08b8bdd010a202020202020000000fc004f\
647973736579204739335344000000ff00484e54594130313431370a202002d0";

    /// Real EDID from an internal laptop panel, which reports serial 0.
    const PANEL: &str = "00ffffffffffff0051a16420000000001b220104a523167803de51a3544c9926\
0f505400000001010101010101010101010101010101606c00a0a04064603020\
680059d710000018000000fd003078cccc38010a202020202020000000100000\
000000000000000000000000000000fc00544c31363041444d5033350a2001a9";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn test_parses_a_real_monitor() {
        let id = parse(&hex(SAMSUNG)).expect("valid EDID should parse");
        assert_eq!(id.mfg, "SAM");
        assert_eq!(id.product, 0x772d);
        assert_eq!(id.serial, Some(811_021_873));
        assert_eq!(id.name, "Odyssey G93SD");
    }

    /// A monitor reporting serial 0 is reporting nothing, so identity falls back
    /// to manufacturer and product. Spec C7.
    #[test]
    fn test_zero_serial_becomes_none() {
        let id = parse(&hex(PANEL)).expect("valid EDID should parse");
        assert_eq!(id.mfg, "TMA");
        assert_eq!(id.serial, None);
        assert_eq!(id.name, "TL160ADMP35");
    }

    #[test]
    fn test_selector_includes_serial_when_present() {
        let id = parse(&hex(SAMSUNG)).unwrap();
        assert_eq!(id.selector(), "SAM:772d:811021873");
    }

    #[test]
    fn test_selector_omits_absent_serial() {
        let id = parse(&hex(PANEL)).unwrap();
        assert_eq!(id.selector(), "TMA:2064");
    }

    #[test]
    fn test_bad_header_is_rejected() {
        let mut b = hex(SAMSUNG);
        b[0] = 0x01;
        assert!(parse(&b).is_none());
    }

    #[test]
    fn test_all_zero_blob_is_rejected() {
        assert!(parse(&[0u8; EDID_BASE_LEN]).is_none());
    }

    /// Every truncation must return None rather than panic. Spec F4.
    #[test]
    fn test_every_truncation_is_rejected_without_panicking() {
        let full = hex(SAMSUNG);
        for len in 0..EDID_BASE_LEN {
            assert!(
                parse(&full[..len]).is_none(),
                "length {len} should not parse"
            );
        }
    }

    /// A valid header followed by arbitrary bytes must not panic, whatever it
    /// parses to. Spec F5.
    #[test]
    fn test_garbage_body_does_not_panic() {
        let mut b = vec![0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        let mut x: u8 = 7;
        while b.len() < EDID_BASE_LEN {
            x = x.wrapping_mul(31).wrapping_add(17);
            b.push(x);
        }
        let _ = parse(&b);
    }

    /// A manufacturer field outside the 1..=26 letter range is not a real EDID.
    #[test]
    fn test_out_of_range_manufacturer_is_rejected() {
        let mut b = hex(SAMSUNG);
        b[8] = 0x00;
        b[9] = 0x00;
        assert!(parse(&b).is_none());
    }

    /// No monitor name descriptor gives an empty name, not a failure. Spec F7.
    #[test]
    fn test_missing_name_descriptor_gives_empty_name() {
        let mut b = hex(SAMSUNG);
        for off in (54..126).step_by(18) {
            b[off + 3] = 0x00;
        }
        let id = parse(&b).expect("still a valid EDID without a name");
        assert_eq!(id.name, "");
    }

    #[test]
    fn test_extra_bytes_beyond_the_base_block_are_ignored() {
        let mut b = hex(SAMSUNG);
        b.extend_from_slice(&[0xAB; 128]);
        let id = parse(&b).expect("extension blocks are ignored, not fatal");
        assert_eq!(id.mfg, "SAM");
    }
}
