#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(signed) = serde_json::from_slice::<
        switchboard::identity::Signed<switchboard::directory::PeerCard>,
    >(data)
    {
        let _ = switchboard::directory::verify_peer_card(signed);
    }
});
