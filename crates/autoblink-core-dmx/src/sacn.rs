//! The sACN (E1.31) data packet: the reader and the multicast group of a universe.

use std::net::Ipv4Addr;

/// The UDP port of sACN.
pub const SACN_PORT: u16 = 5568;

const ACN_ID: &[u8; 12] = b"ASC-E1.17\0\0\0";
const VECTOR_ROOT_DATA: u32 = 0x0000_0004;
const VECTOR_FRAMING_DATA: u32 = 0x0000_0002;
const OPT_PREVIEW: u8 = 0x80;
const OPT_TERMINATED: u8 = 0x40;
/// The offset of the property values. The first value is the start code.
const VALUES: usize = 125;

/// One sACN data packet.
#[derive(Debug, PartialEq, Eq)]
pub struct SacnDmx<'a> {
    /// The universe, 1 to 63999.
    pub universe: u16,
    /// The priority of the source, 0 to 200.
    pub priority: u8,
    /// The sequence number of the packet.
    pub sequence: u8,
    /// The source ends the stream (Options bit 6). The caller ignores `data` then.
    pub terminated: bool,
    /// The slots after the start code, 0 to 512 bytes.
    pub data: &'a [u8],
    /// The CID of the source. Two sources on one computer have different CIDs.
    pub cid: [u8; 16],
}

/// Returns the data of an sACN data packet, or None.
///
/// The reader drops a packet with the preview flag, a start code other than 0 and each
/// packet that is not a data packet (synchronization and universe discovery).
///
/// ponytail: the reader does not check the flags and length fields of the three layers. It
/// trusts the property value count and the packet length. The upgrade is to compare each
/// length field with the packet length.
pub fn parse_sacn(packet: &[u8]) -> Option<SacnDmx<'_>> {
    if packet.len() < VALUES {
        return None;
    }
    if u16::from_be_bytes([packet[0], packet[1]]) != 0x0010
        || packet[2..4] != [0, 0]
        || &packet[4..16] != ACN_ID
    {
        return None;
    }
    if be32(&packet[18..22]) != VECTOR_ROOT_DATA || be32(&packet[40..44]) != VECTOR_FRAMING_DATA {
        return None;
    }
    let priority = packet[108];
    let options = packet[112];
    let universe = u16::from_be_bytes([packet[113], packet[114]]);
    if priority > 200 || options & OPT_PREVIEW != 0 || !(1..=63999).contains(&universe) {
        return None;
    }
    if packet[117] != 0x02
        || packet[118] != 0xa1
        || packet[119..121] != [0, 0]
        || packet[121..123] != [0, 1]
    {
        return None;
    }
    let count = usize::from(u16::from_be_bytes([packet[123], packet[124]]));
    if !(1..=513).contains(&count) {
        return None;
    }
    let values = packet.get(VALUES..VALUES + count)?;
    let terminated = options & OPT_TERMINATED != 0;
    let data = if terminated {
        &[][..]
    } else if values[0] == 0 {
        &values[1..]
    } else {
        return None;
    };
    Some(SacnDmx {
        universe,
        priority,
        sequence: packet[111],
        terminated,
        data,
        cid: packet[22..38].try_into().ok()?,
    })
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Returns the multicast group of `universe`: 239.255.(universe high byte).(universe low byte).
pub fn multicast_group(universe: u16) -> Ipv4Addr {
    Ipv4Addr::new(239, 255, (universe >> 8) as u8, universe as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(universe: u16, start: u8, slots: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; VALUES];
        p[..2].copy_from_slice(&0x0010u16.to_be_bytes());
        p[4..16].copy_from_slice(ACN_ID);
        p[18..22].copy_from_slice(&VECTOR_ROOT_DATA.to_be_bytes());
        p[40..44].copy_from_slice(&VECTOR_FRAMING_DATA.to_be_bytes());
        p[108] = 100;
        p[111] = 7;
        p[113..115].copy_from_slice(&universe.to_be_bytes());
        p[117] = 0x02;
        p[118] = 0xa1;
        p[121..123].copy_from_slice(&1u16.to_be_bytes());
        p[123..125].copy_from_slice(&(slots.len() as u16 + 1).to_be_bytes());
        p.push(start);
        p.extend(slots);
        p
    }

    #[test]
    fn parse_valid() {
        let p = packet(5, 0, &[1, 2, 3]);
        let s = parse_sacn(&p).unwrap();
        assert_eq!(s.universe, 5);
        assert_eq!(s.priority, 100);
        assert_eq!(s.sequence, 7);
        assert!(!s.terminated);
        assert_eq!(s.data, &[1, 2, 3]);
        assert_eq!(
            parse_sacn(&packet(1, 0, &[9; 512])).unwrap().data.len(),
            512
        );
        assert_eq!(parse_sacn(&packet(63999, 0, &[])).unwrap().data.len(), 0);
    }

    #[test]
    fn parse_rejects() {
        let good = packet(1, 0, &[1, 2]);
        let bad = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut p = good.clone();
            f(&mut p);
            assert!(parse_sacn(&p).is_none());
        };
        bad(&|p| p[4] = b'X');
        bad(&|p| p[21] = 8);
        bad(&|p| p[43] = 1);
        bad(&|p| p[112] = 0x80);
        bad(&|p| p[125] = 0xdd);
        bad(&|p| p[124] = 0);
        bad(&|p| p[123..125].copy_from_slice(&514u16.to_be_bytes()));
        bad(&|p| p.truncate(126));
        bad(&|p| p.truncate(100));
        bad(&|p| p[113..115].copy_from_slice(&0u16.to_be_bytes()));
        bad(&|p| p[113..115].copy_from_slice(&64000u16.to_be_bytes()));
        bad(&|p| p[108] = 201);
        assert!(parse_sacn(&packet(1, 0, &[0; 512])).is_some());
    }

    #[test]
    fn terminated_packet() {
        let mut p = packet(1, 0xdd, &[1, 2]);
        p[112] = 0x40;
        let s = parse_sacn(&p).unwrap();
        assert!(s.terminated);
        assert!(s.data.is_empty());
    }

    #[test]
    fn multicast() {
        assert_eq!(multicast_group(1), Ipv4Addr::new(239, 255, 0, 1));
        assert_eq!(multicast_group(0x1234), Ipv4Addr::new(239, 255, 0x12, 0x34));
    }
}
