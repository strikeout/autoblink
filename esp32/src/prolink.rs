use smoltcp::iface::{EthernetInterface, Interface, SocketStorage};
use smoltcp::socket::UdpSocket;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};
use xdj_core_prolink::{
    pitch_to_percent, BPM_NONE, MAGIC, PKT_ABS_POSITION, PKT_ANNOUNCE, PKT_BEAT, PKT_CDJ_STATUS,
    PKT_KEEPALIVE, PKT_MIXER_STATUS, PORT_DISCOVERY,
};

pub struct ProLinkStack {
    iface: Interface,
    udp_socket: UdpSocket,
}

impl ProLinkStack {
    pub fn new(mac: [u8; 6]) -> anyhow::Result<Self> {
        let eth_addr = EthernetAddress(mac);
        let ip_addr = IpAddress::v4(169, 254, 100, 50);
        let ip_cidr = IpCidr::new(ip_addr, 16);

        let mut storage = [0u8; 4096];
        let mut iface = Interface::new(eth_addr, &mut storage);

        iface.update_ip_infallible(ip_cidr);
        iface.set_any_ip(true);

        Ok(Self {
            iface,
            udp_socket: UdpSocket::new(),
        })
    }

    pub fn process_packet(&mut self, packet: &[u8]) -> Option<ProLinkPacket> {
        if packet.len() < 11 || &packet[..10] != MAGIC {
            return None;
        }
        let pkt_type = packet[10];
        match pkt_type {
            PKT_BEAT => self.parse_beat_packet(packet),
            PKT_ABS_POSITION => self.parse_abs_position(packet),
            PKT_CDJ_STATUS => self.parse_cdj_status(packet),
            PKT_MIXER_STATUS => self.parse_mixer_status(packet),
            PKT_KEEPALIVE => Some(ProLinkPacket::KeepAlive),
            _ => None,
        }
    }

    fn parse_beat_packet(&self, packet: &[u8]) -> Option<ProLinkPacket> {
        if packet.len() < 0x60 {
            return None;
        }
        let bpm_raw = u16::from_be_bytes([packet[0x5a], packet[0x5b]]);
        let track_bpm = if bpm_raw == BPM_NONE {
            0.0
        } else {
            bpm_raw as f64 / 100.0
        };
        let pitch_raw =
            u32::from_be_bytes([packet[0x54], packet[0x55], packet[0x56], packet[0x57]]);
        let pitch_pct = pitch_to_percent(pitch_raw);
        let eff_bpm = if track_bpm > 0.0 {
            track_bpm * (1.0 + pitch_pct / 100.0)
        } else {
            0.0
        };

        Some(ProLinkPacket::Beat(BeatPacket {
            device_number: packet[0x21],
            effective_bpm: eff_bpm,
            beat_in_bar: packet[0x5c],
            pitch_pct,
        }))
    }

    fn parse_abs_position(&self, packet: &[u8]) -> Option<ProLinkPacket> {
        if packet.len() < 0x3c {
            return None;
        }
        let bpm_x10 = u32::from_be_bytes([packet[0x38], packet[0x39], packet[0x3a], packet[0x3b]]);
        let pitch_raw =
            i32::from_be_bytes([packet[0x2c], packet[0x2d], packet[0x2e], packet[0x2f]]);
        Some(ProLinkPacket::AbsPosition(AbsPositionPacket {
            device_number: packet[0x21],
            playhead_ms: u32::from_be_bytes([
                packet[0x28],
                packet[0x29],
                packet[0x2a],
                packet[0x2b],
            ]),
            effective_bpm: bpm_x10 as f64 / 10.0,
            pitch_pct: pitch_raw as f64 / 100.0,
        }))
    }

    fn parse_cdj_status(&self, packet: &[u8]) -> Option<ProLinkPacket> {
        if packet.len() < 0xd4 {
            return None;
        }
        let state = u16::from_be_bytes([packet[0x88], packet[0x89]]);
        let is_playing = (state & 0x0040) != 0;
        let is_master = (state & 0x0020) != 0;

        let bpm_raw = u16::from_be_bytes([packet[0x92], packet[0x93]]);
        let track_bpm = if bpm_raw == BPM_NONE {
            0.0
        } else {
            bpm_raw as f64 / 100.0
        };

        let pitch_raw =
            u32::from_be_bytes([packet[0x98], packet[0x99], packet[0x9a], packet[0x9b]]);
        let pitch_pct = pitch_to_percent(pitch_raw);
        let eff_bpm = if track_bpm > 0.0 {
            track_bpm * (1.0 + pitch_pct / 100.0)
        } else {
            0.0
        };

        Some(ProLinkPacket::CdjStatus(CdjStatus {
            device_number: packet[0x21],
            play_state: packet[0x7b],
            is_playing,
            is_master,
            effective_bpm: eff_bpm,
            pitch_pct,
            beat_in_bar: packet[0xa6],
        }))
    }

    fn parse_mixer_status(&self, packet: &[u8]) -> Option<ProLinkPacket> {
        if packet.len() < 0x38 {
            return None;
        }
        let state = u16::from_be_bytes([packet[0x26], packet[0x27]]);
        let is_master = (state & 0x0020) != 0;
        let bpm_raw = u16::from_be_bytes([packet[0x2e], packet[0x2f]]);

        Some(ProLinkPacket::MixerStatus(MixerStatus {
            device_number: packet[0x21],
            is_master,
            track_bpm: if bpm_raw == BPM_NONE {
                None
            } else {
                Some(bpm_raw as f64 / 100.0)
            },
        }))
    }
}

#[derive(Debug)]
pub enum ProLinkPacket {
    Beat(BeatPacket),
    AbsPosition(AbsPositionPacket),
    CdjStatus(CdjStatus),
    MixerStatus(MixerStatus),
    KeepAlive,
}

#[derive(Debug)]
pub struct BeatPacket {
    pub device_number: u8,
    pub effective_bpm: f64,
    pub beat_in_bar: u8,
    pub pitch_pct: f64,
}

#[derive(Debug)]
pub struct AbsPositionPacket {
    pub device_number: u8,
    pub playhead_ms: u32,
    pub effective_bpm: f64,
    pub pitch_pct: f64,
}

#[derive(Debug)]
pub struct CdjStatus {
    pub device_number: u8,
    pub play_state: u8,
    pub is_playing: bool,
    pub is_master: bool,
    pub effective_bpm: f64,
    pub pitch_pct: f64,
    pub beat_in_bar: u8,
}

#[derive(Debug)]
pub struct MixerStatus {
    pub device_number: u8,
    pub is_master: bool,
    pub track_bpm: Option<f64>,
}

impl ProLinkStack {
    pub fn send_announce(&mut self, device_number: u8, device_name: &str) {
        let mut packet = [0u8; 64];
        packet[..10].copy_from_slice(&MAGIC);
        packet[10] = PKT_ANNOUNCE;
        packet[11] = device_number;
        let name_bytes = device_name.as_bytes();
        let len = name_bytes.len().min(16);
        packet[12..12 + len].copy_from_slice(&name_bytes[..len]);
        let _ = self.udp_socket.send_to(
            &packet[..12 + len],
            IpAddress::v4(255, 255, 255, 255),
            PORT_DISCOVERY,
        );
    }
}
