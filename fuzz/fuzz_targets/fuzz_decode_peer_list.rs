#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // decode_peer_list must never panic on arbitrary input.
    let _ = donglora_bridge::gossip::decode_peer_list(data);
});
