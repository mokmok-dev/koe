//! CRC-32/ISO-HDLC, the checksum a whole record carries.

const POLYNOMIAL: u32 = 0xEDB8_8320;
const INIT: u32 = 0xFFFF_FFFF;

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = INIT;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ POLYNOMIAL
            };
        }
    }
    !crc
}
