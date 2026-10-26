use std::net::UdpSocket;
use std::time::Duration;
use autoblink_core_prolink::{MAGIC, PKT_ABS_POSITION, PKT_MIXER_STATUS};

fn main() -> anyhow::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    let target = "127.0.0.1:50001"; // AbsPosition port
    let target_status = "127.0.0.1:50002"; // Status port

    println!("Starting Pro DJ Link Simulator (Stopped Deck Status)...");

    loop {
        // 1. Send AbsPosition packet (CDJ-3000 style)
        // Offset 0x21: device_num (1)
        // Offset 0x24: track_length_s (300)
        // Offset 0x28: playhead_ms (5000)
        // Offset 0x2c: pitch_raw_signed (123 -> 1.23%)
        // Offset 0x38: bpm_x10 (1245 -> 124.5 BPM)
        let mut abs_pkt = vec![0u8; 0x3c];
        abs_pkt[..10].copy_from_slice(&MAGIC);
        abs_pkt[10] = PKT_ABS_POSITION;
        abs_pkt[0x21] = 1;
        abs_pkt[0x24..0x28].copy_from_slice(&300u32.to_be_bytes());
        abs_pkt[0x28..0x2c].copy_from_slice(&5000u32.to_be_bytes());
        abs_pkt[0x2c..0x30].copy_from_slice(&123i32.to_be_bytes());
        abs_pkt[0x38..0x3c].copy_from_slice(&1245u32.to_be_bytes());

        sock.send_to(&abs_pkt, target)?;
        println!("Sent AbsPosition: Deck #1, 124.5 BPM, 1.23% Pitch");

        // 2. Send Mixer Status packet
        // Offset 0x21: device_num (33)
        // Offset 0x26: state (master = 0x0020)
        // Offset 0x2e: bpm_raw (12800 -> 128.00 BPM)
        let mut mixer_pkt = vec![0u8; 0x38];
        mixer_pkt[..10].copy_from_slice(&MAGIC);
        mixer_pkt[10] = PKT_MIXER_STATUS;
        mixer_pkt[0x21] = 33;
        mixer_pkt[0x26..0x28].copy_from_slice(&0x0020u16.to_be_bytes());
        mixer_pkt[0x2e..0x30].copy_from_slice(&12800u16.to_be_bytes());

        sock.send_to(&mixer_pkt, target_status)?;
        println!("Sent Mixer Status: #33 MASTER, 128.00 BPM");

        std::thread::sleep(Duration::from_millis(1000));
    }
}
