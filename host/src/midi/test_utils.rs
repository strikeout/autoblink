use crate::midi::transport::MidiResult;
use crate::midi::{MidiError, MidiTransport};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct MockMidiTransport {
    messages: Arc<Mutex<Vec<Vec<u8>>>>,
    should_fail: bool,
    connected: Arc<AtomicBool>,
}

impl MockMidiTransport {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(Vec::new())),
            should_fail: false,
            connected: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn get_messages(&self) -> Vec<Vec<u8>> {
        self.messages.lock().unwrap().clone()
    }

    pub fn clear_messages(&self) {
        self.messages.lock().unwrap().clear();
    }

    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }
}

impl MidiTransport for MockMidiTransport {
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn send_message(&self, msg: &[u8]) -> MidiResult {
        if self.should_fail {
            return Err(MidiError::SendError("Mock failure".to_string()));
        }
        self.messages.lock().unwrap().push(msg.to_vec());
        Ok(())
    }
}
