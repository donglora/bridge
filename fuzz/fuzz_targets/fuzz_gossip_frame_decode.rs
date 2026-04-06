#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // GossipFrame::decode must never panic on arbitrary input.
    let _ = donglora_bridge::packet::GossipFrame::decode(data);
});
