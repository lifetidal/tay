#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| tayd::fuzz_support::sixel_dcs(data));
