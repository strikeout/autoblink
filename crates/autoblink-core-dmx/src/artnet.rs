//! The Art-Net packets: ArtDmx (the reader and the writer), ArtPoll and ArtPollReply.

use crate::render::Universe;
use crate::show::UNIVERSE;

/// The UDP port of Art-Net.
pub const ARTNET_PORT: u16 = 6454;

const ID: &[u8; 8] = b"Art-Net\0";
const OP_DMX: u16 = 0x5000;
const OP_POLL: u16 = 0x2000;
const OP_POLL_REPLY: u16 = 0x2100;
const HEADER: usize = 18;

/// The length of an Art-Net 4 ArtPollReply.
pub const POLL_REPLY_LEN: usize = 239;

/// Returns true when `packet` is an ArtPoll. A desk sends it to find the nodes.
pub fn is_artpoll(packet: &[u8]) -> bool {
    parse_artpoll(packet).is_some()
}

/// An ArtPoll packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtPoll {
    /// The flags byte of the controller.
    pub flags: u8,
    /// The bottom and the top port-address of a targeted poll.
    pub target: Option<(u16, u16)>,
}

impl ArtPoll {
    /// Returns true when the node with `port_address` must reply.
    pub fn wants(&self, port_address: u16) -> bool {
        match self.target {
            None => true,
            Some((bottom, top)) => (bottom..=top).contains(&port_address),
        }
    }

    /// Returns true when the controller wants an ArtPollReply after each change of the node
    /// (flags bit 1).
    pub fn reply_on_change(&self) -> bool {
        self.flags & 0x02 != 0
    }
}

/// Returns the fields of an ArtPoll packet, or None.
pub fn parse_artpoll(packet: &[u8]) -> Option<ArtPoll> {
    if packet.len() < 12 || &packet[..8] != ID || packet[8..10] != OP_POLL.to_le_bytes() {
        return None;
    }
    let flags = packet.get(12).copied().unwrap_or(0);
    // Flags bit 5: the controller targets a range of port-addresses.
    let target = if flags & 0x20 != 0 && packet.len() >= 18 {
        let top = u16::from_be_bytes([packet[14], packet[15]]);
        let bottom = u16::from_be_bytes([packet[16], packet[17]]);
        Some((bottom, top))
    } else {
        None
    };
    Some(ArtPoll { flags, target })
}

/// The fields of the node that an ArtPollReply describes.
pub struct NodeInfo<'a> {
    /// The IPv4 address of the interface that answers.
    pub ip: [u8; 4],
    /// The MAC address of the node.
    pub mac: [u8; 6],
    /// The major and the minor version of the firmware.
    pub version: (u8, u8),
    /// The short name. The reply cuts it at 17 bytes.
    pub short_name: &'a str,
    /// The long name. The reply cuts it at 63 bytes.
    pub long_name: &'a str,
    /// The node report, for example "#0001 [nnnn] text". The reply cuts it at 63 bytes.
    pub report: &'a str,
    /// The 15-bit port-address (the universe) of the one output port.
    pub port_address: u16,
    /// The DMX output sends frames.
    pub sending: bool,
    /// The output takes its data from sACN, not from Art-Net.
    pub sacn: bool,
    /// The output merges a network source with the box.
    pub merging: bool,
    /// The output sends continuously. Else it sends only on a change.
    pub continuous: bool,
    /// The address of this interface came from DHCP.
    pub dhcp: bool,
    /// On a loss of data the output goes to zero. Else it holds the last frame.
    pub failsafe_zero: bool,
}

/// Writes an ArtPollReply for a node with one DMX output.
pub fn build_artpoll_reply(node: &NodeInfo) -> [u8; POLL_REPLY_LEN] {
    let mut out = [0u8; POLL_REPLY_LEN];
    let pa = node.port_address;
    out[..8].copy_from_slice(ID);
    out[8..10].copy_from_slice(&OP_POLL_REPLY.to_le_bytes());
    out[10..14].copy_from_slice(&node.ip);
    out[14..16].copy_from_slice(&ARTNET_PORT.to_le_bytes());
    out[16] = node.version.0;
    out[17] = node.version.1;
    out[18] = (pa >> 8) as u8 & 0x7f;
    out[19] = (pa >> 4) as u8 & 0x0f;
    // Oem: unknown.
    out[21] = 0xff;
    // Status1: the indicators are normal, and the web page sets the port-address.
    out[23] = 0xe0;
    // ESTA manufacturer code 0x7FF0 is for prototypes. The field is low byte first.
    out[24..26].copy_from_slice(&0x7ff0u16.to_le_bytes());
    copy_text(&mut out[26..44], node.short_name);
    copy_text(&mut out[44..108], node.long_name);
    copy_text(&mut out[108..172], node.report);
    // One port. It sends DMX512 (PortTypes bit 7).
    out[173] = 1;
    out[174] = 0x80;
    // GoodOutputA: bit 7 sends, bit 1 merges, bit 0 takes sACN.
    out[182] = u8::from(node.sending) << 7 | u8::from(node.merging) << 3 | u8::from(node.sacn);
    out[190] = pa as u8 & 0x0f;
    out[201..207].copy_from_slice(&node.mac);
    out[207..211].copy_from_slice(&node.ip);
    out[211] = 1;
    // Status2: web configuration, DHCP capable, 15-bit port-addresses. Bit 1 is DHCP in use.
    out[212] = 0x0d | u8::from(node.dhcp) << 1;
    // GoodOutputB: RDM is off, and the discovery does not run. Bit 6 is continuous output.
    out[213] = 0xb0 | u8::from(node.continuous) << 6;
    // Status3 bits 7 and 6: the failsafe. 00 holds the last frame, 01 goes to zero.
    out[217] = u8::from(node.failsafe_zero) << 6;
    // RefreshRate: the output sends at most 41 frames each second.
    out[226..228].copy_from_slice(&41u16.to_be_bytes());
    out
}

/// Copies `text` into `field` and keeps the last byte 0.
fn copy_text(field: &mut [u8], text: &str) {
    let n = text.len().min(field.len() - 1);
    field[..n].copy_from_slice(&text.as_bytes()[..n]);
}

/// Returns the universe (Net << 8 | SubUni) and the data of an ArtDmx packet, or None.
pub fn parse_artdmx(packet: &[u8]) -> Option<(u16, &[u8])> {
    if packet.len() < HEADER || &packet[..8] != ID {
        return None;
    }
    if u16::from_le_bytes([packet[8], packet[9]]) != OP_DMX {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([packet[16], packet[17]]));
    if len == 0 || len > UNIVERSE {
        return None;
    }
    let data = packet.get(HEADER..HEADER + len)?;
    let universe = u16::from(packet[15] & 0x7f) << 8 | u16::from(packet[14]);
    Some((universe, data))
}

/// Writes an ArtDmx packet for `universe` with `data` (512 bytes) into `out`. Returns the length.
pub fn build_artdmx(sequence: u8, universe: u16, data: &Universe, out: &mut [u8; 530]) -> usize {
    out[..8].copy_from_slice(ID);
    out[8..10].copy_from_slice(&OP_DMX.to_le_bytes());
    out[10..12].copy_from_slice(&14u16.to_be_bytes());
    out[12] = sequence;
    out[13] = 0;
    out[14] = universe as u8;
    out[15] = (universe >> 8) as u8 & 0x7f;
    out[16..18].copy_from_slice(&(UNIVERSE as u16).to_be_bytes());
    out[HEADER..].copy_from_slice(data);
    HEADER + UNIVERSE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(universe: u16, data: &[u8]) -> Vec<u8> {
        let mut p = b"Art-Net\0".to_vec();
        p.extend([
            0x00,
            0x50,
            0,
            14,
            0,
            0,
            universe as u8,
            (universe >> 8) as u8,
        ]);
        p.extend((data.len() as u16).to_be_bytes());
        p.extend(data);
        p
    }

    #[test]
    fn parse_valid_artdmx() {
        let p = packet(0x0102, &[1, 2, 3, 4]);
        assert_eq!(parse_artdmx(&p), Some((0x0102, &[1u8, 2, 3, 4][..])));
    }

    #[test]
    fn parse_rejects() {
        let mut p = packet(0, &[1, 2]);
        p[0] = b'X';
        assert!(parse_artdmx(&p).is_none());
        let mut p = packet(0, &[1, 2]);
        p[9] = 0x21;
        assert!(parse_artdmx(&p).is_none());
        assert!(parse_artdmx(&packet(0, &[1, 2])[..17]).is_none());
        let mut p = packet(0, &[1, 2]);
        p[17] = 9;
        assert!(parse_artdmx(&p).is_none());
        assert!(parse_artdmx(&packet(0, &[0; 513])).is_none());
        assert!(parse_artdmx(&packet(0, &[])).is_none());
        assert!(parse_artdmx(&packet(0, &[0; 512])).is_some());
        assert!(parse_artdmx(&packet(0, &[1])).is_some());
    }

    #[test]
    fn artpoll_detect() {
        let mut p = b"Art-Net\0\x00\x20\x00\x0e\x00\x00".to_vec();
        assert!(is_artpoll(&p));
        p[9] = 0x50;
        assert!(!is_artpoll(&p));
        assert!(!is_artpoll(&b"Art-Net\0\x00\x20"[..]));
    }

    fn node() -> NodeInfo<'static> {
        NodeInfo {
            ip: [192, 168, 1, 104],
            mac: [1, 2, 3, 4, 5, 6],
            version: (1, 30),
            short_name: "AUTOBLINK",
            long_name: "AUTOBLINK beat-driven DMX box",
            report: "#0001 [0] OK",
            port_address: 0,
            sending: false,
            sacn: false,
            merging: false,
            continuous: false,
            dhcp: false,
            failsafe_zero: false,
        }
    }

    #[test]
    fn artpoll_reply_fields() {
        let r = build_artpoll_reply(&node());
        assert_eq!(&r[..8], b"Art-Net\0");
        assert_eq!(r[8..10], [0x00, 0x21]);
        assert_eq!(r[10..14], [192, 168, 1, 104]);
        assert_eq!(r[14..16], [0x36, 0x19]);
        assert_eq!(r[16..18], [1, 30]);
        assert_eq!(r[20..22], [0, 0xff]);
        assert_eq!(r[23], 0xe0);
        assert_eq!(r[24..26], [0xf0, 0x7f]);
        assert_eq!(&r[26..36], b"AUTOBLINK\0");
        assert_eq!(&r[44..48], b"AUTO");
        assert_eq!(&r[108..120], b"#0001 [0] OK");
        assert_eq!(r[172..175], [0, 1, 0x80]);
        assert_eq!(r[182], 0);
        assert_eq!(r[190], 0);
        assert_eq!(r[200], 0);
        assert_eq!(r[201..207], [1, 2, 3, 4, 5, 6]);
        assert_eq!(r[207..211], [192, 168, 1, 104]);
        assert_eq!(r[211], 1);
        assert_eq!(r[212], 0x0d);
        assert_eq!(r[213], 0xb0);
        assert_eq!(r[217], 0);
        assert_eq!(r[226..228], [0, 41]);
        let long = "x".repeat(100);
        let r = build_artpoll_reply(&NodeInfo {
            report: &long,
            long_name: &long,
            short_name: &long,
            ..node()
        });
        assert_eq!(r[43], 0);
        assert_eq!(r[42], b'x');
        assert_eq!(r[107], 0);
        assert_eq!(r[171], 0);
        assert_eq!(r[170], b'x');
    }

    #[test]
    fn artpoll_reply_port_address() {
        let r = build_artpoll_reply(&NodeInfo {
            port_address: 0x1234,
            ..node()
        });
        assert_eq!(r[18], 0x12);
        assert_eq!(r[19], 0x3);
        assert_eq!(r[190], 0x4);
    }

    #[test]
    fn artpoll_reply_flags() {
        let r = build_artpoll_reply(&NodeInfo {
            sending: true,
            sacn: true,
            merging: true,
            continuous: true,
            dhcp: true,
            failsafe_zero: true,
            ..node()
        });
        assert_eq!(r[182], 0x89);
        assert_eq!(r[212], 0x0f);
        assert_eq!(r[213], 0xf0);
        assert_eq!(r[217], 0x40);
        let r = build_artpoll_reply(&NodeInfo {
            sending: true,
            ..node()
        });
        assert_eq!(r[182], 0x80);
        let r = build_artpoll_reply(&NodeInfo {
            merging: true,
            ..node()
        });
        assert_eq!(r[182], 0x08);
    }

    #[test]
    fn artpoll_parse() {
        let mut p = b"Art-Net\0\x00\x20\x00\x0e".to_vec();
        // Minimum length: the flags are 0.
        let a = parse_artpoll(&p).unwrap();
        assert_eq!(a.flags, 0);
        assert_eq!(a.target, None);
        assert!(!a.reply_on_change());
        p.extend([0x02, 0]);
        assert!(parse_artpoll(&p).unwrap().reply_on_change());
        // Targeted, but too short for the range.
        p[12] = 0x22;
        assert_eq!(parse_artpoll(&p).unwrap().target, None);
        p.extend([0x00, 0x20, 0x00, 0x10]);
        let a = parse_artpoll(&p).unwrap();
        assert_eq!(a.target, Some((0x10, 0x20)));
        assert!(a.wants(0x10) && a.wants(0x20) && a.wants(0x18));
        assert!(!a.wants(0x0f) && !a.wants(0x21));
        // Flag bit 5 off: the range is ignored.
        p[12] = 0;
        let a = parse_artpoll(&p).unwrap();
        assert_eq!(a.target, None);
        assert!(a.wants(0x7fff));
        assert!(parse_artpoll(&p[..11]).is_none());
    }

    #[test]
    fn build_round_trip() {
        let mut data = [0u8; UNIVERSE];
        data[0] = 7;
        data[511] = 9;
        let mut out = [0u8; 530];
        let n = build_artdmx(3, 0x0102, &data, &mut out);
        assert_eq!(n, 530);
        let (u, d) = parse_artdmx(&out[..n]).unwrap();
        assert_eq!(u, 0x0102);
        assert_eq!(d, &data[..]);
        assert_eq!(out[12], 3);
        assert_eq!(out[10..12], [0, 14]);
        assert_eq!(out[13], 0);
        assert_eq!(out[16..18], [2, 0]);
    }
}
