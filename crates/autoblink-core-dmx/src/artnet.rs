//! The Art-Net ArtDmx packet: the reader and the writer.

use crate::render::Universe;
use crate::show::UNIVERSE;

/// The UDP port of Art-Net.
pub const ARTNET_PORT: u16 = 6454;

const ID: &[u8; 8] = b"Art-Net\0";
const OP_DMX: u16 = 0x5000;
const HEADER: usize = 18;

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
