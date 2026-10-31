//! QQ Music's encrypted QRC lyrics: a QMC1 mask over triple DES over zlib.
//!
//! The cipher is DES with QQ Music's own bit layout, so no standard DES library fits.
//! Ported from jixunmoe-go/qrc, with the request flow from better-lyrics/api (GPL-3.0).
//! The notice below is the MIT license of jixunmoe-go/qrc.

// MIT License
//
// Copyright (c) 2023 Jixun Wu
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::io::Read;

const QMC_MAGIC: [u8; 11] = [0x98, 0x25, 0xB0, 0xAC, 0xE3, 0x02, 0x83, 0x68, 0xE8, 0xFC, 0x6C];
const KEYS: [(&[u8; 8], bool); 3] = [(b"!@#)(NHL", false), (b"123ZXC!@", true), (b"!@#)(*$%", false)];

const KEY_RND_SHIFTS: [u8; 16] = [
    0x01, 0x01, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x01, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x01,
];
const LARGE_STATE_SHIFTS: [u8; 8] = [
    0x1a, 0x14, 0x0e, 0x08, 0x3a, 0x34, 0x2e, 0x28,
];
const P_BOX: [u8; 32] = [
    0x0f, 0x06, 0x13, 0x14, 0x1c, 0x0b, 0x1b, 0x10, 0x00, 0x0e, 0x16, 0x19, 0x04, 0x11, 0x1e, 0x09,
    0x01, 0x07, 0x17, 0x0d, 0x1f, 0x1a, 0x02, 0x08, 0x12, 0x0c, 0x1d, 0x05, 0x15, 0x0a, 0x03, 0x18,
];
const IP: [u8; 64] = [
    0x39, 0x31, 0x29, 0x21, 0x19, 0x11, 0x09, 0x01, 0x3b, 0x33, 0x2b, 0x23, 0x1b, 0x13, 0x0b, 0x03,
    0x3d, 0x35, 0x2d, 0x25, 0x1d, 0x15, 0x0d, 0x05, 0x3f, 0x37, 0x2f, 0x27, 0x1f, 0x17, 0x0f, 0x07,
    0x38, 0x30, 0x28, 0x20, 0x18, 0x10, 0x08, 0x00, 0x3a, 0x32, 0x2a, 0x22, 0x1a, 0x12, 0x0a, 0x02,
    0x3c, 0x34, 0x2c, 0x24, 0x1c, 0x14, 0x0c, 0x04, 0x3e, 0x36, 0x2e, 0x26, 0x1e, 0x16, 0x0e, 0x06,
];
const IP_INV: [u8; 64] = [
    0x27, 0x07, 0x2f, 0x0f, 0x37, 0x17, 0x3f, 0x1f, 0x26, 0x06, 0x2e, 0x0e, 0x36, 0x16, 0x3e, 0x1e,
    0x25, 0x05, 0x2d, 0x0d, 0x35, 0x15, 0x3d, 0x1d, 0x24, 0x04, 0x2c, 0x0c, 0x34, 0x14, 0x3c, 0x1c,
    0x23, 0x03, 0x2b, 0x0b, 0x33, 0x13, 0x3b, 0x1b, 0x22, 0x02, 0x2a, 0x0a, 0x32, 0x12, 0x3a, 0x1a,
    0x21, 0x01, 0x29, 0x09, 0x31, 0x11, 0x39, 0x19, 0x20, 0x00, 0x28, 0x08, 0x30, 0x10, 0x38, 0x18,
];
const KEY_PERMUTATION_TABLE: [u8; 56] = [
    0x38, 0x30, 0x28, 0x20, 0x18, 0x10, 0x08, 0x00, 0x39, 0x31, 0x29, 0x21, 0x19, 0x11, 0x09, 0x01,
    0x3a, 0x32, 0x2a, 0x22, 0x1a, 0x12, 0x0a, 0x02, 0x3b, 0x33, 0x2b, 0x23, 0x3e, 0x36, 0x2e, 0x26,
    0x1e, 0x16, 0x0e, 0x06, 0x3d, 0x35, 0x2d, 0x25, 0x1d, 0x15, 0x0d, 0x05, 0x3c, 0x34, 0x2c, 0x24,
    0x1c, 0x14, 0x0c, 0x04, 0x1b, 0x13, 0x0b, 0x03,
];
const KEY_COMPRESSION: [u8; 48] = [
    0x0d, 0x10, 0x0a, 0x17, 0x00, 0x04, 0x02, 0x1b, 0x0e, 0x05, 0x14, 0x09, 0x16, 0x12, 0x0b, 0x03,
    0x19, 0x07, 0x0f, 0x06, 0x1a, 0x13, 0x0c, 0x01, 0x2d, 0x38, 0x23, 0x29, 0x33, 0x3b, 0x22, 0x2c,
    0x37, 0x31, 0x25, 0x34, 0x30, 0x35, 0x2b, 0x3c, 0x26, 0x39, 0x32, 0x2e, 0x36, 0x28, 0x21, 0x24,
];
const KEY_EXPANSION: [u8; 48] = [
    0x1f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x07, 0x08, 0x09, 0x0a,
    0x0b, 0x0c, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x13, 0x14,
    0x15, 0x16, 0x17, 0x18, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x00,
];
const QMC_KEY: [u8; 128] = [
    0xc3, 0x4a, 0xd6, 0xca, 0x90, 0x67, 0xf7, 0x52, 0xd8, 0xa1, 0x66, 0x62, 0x9f, 0x5b, 0x09, 0x00,
    0xc3, 0x5e, 0x95, 0x23, 0x9f, 0x13, 0x11, 0x7e, 0xd8, 0x92, 0x3f, 0xbc, 0x90, 0xbb, 0x74, 0x0e,
    0xc3, 0x47, 0x74, 0x3d, 0x90, 0xaa, 0x3f, 0x51, 0xd8, 0xf4, 0x11, 0x84, 0x9f, 0xde, 0x95, 0x1d,
    0xc3, 0xc6, 0x09, 0xd5, 0x9f, 0xfa, 0x66, 0xf9, 0xd8, 0xf0, 0xf7, 0xa0, 0x90, 0xa1, 0xd6, 0xf3,
    0xc3, 0xf3, 0xd6, 0xa1, 0x90, 0xa0, 0xf7, 0xf0, 0xd8, 0xf9, 0x66, 0xfa, 0x9f, 0xd5, 0x09, 0xc6,
    0xc3, 0x1d, 0x95, 0xde, 0x9f, 0x84, 0x11, 0xf4, 0xd8, 0x51, 0x3f, 0xaa, 0x90, 0x3d, 0x74, 0x47,
    0xc3, 0x0e, 0x74, 0xbb, 0x90, 0xbc, 0x3f, 0x92, 0xd8, 0x7e, 0x11, 0x13, 0x9f, 0x23, 0x95, 0x5e,
    0xc3, 0x00, 0x09, 0x5b, 0x9f, 0x62, 0x66, 0xa1, 0xd8, 0x52, 0xf7, 0x67, 0x90, 0xca, 0xd6, 0x4a,
];
const SBOXES: [[u8; 64]; 8] = [
    [14, 0, 4, 15, 13, 7, 1, 4, 2, 14, 15, 2, 11, 13, 8, 1, 3, 10, 10, 6, 6, 12, 12, 11, 5, 9, 9, 5, 0, 3, 7, 8, 4, 15, 1, 12, 14, 8, 8, 2, 13, 4, 6, 9, 2, 1, 11, 7, 15, 5, 12, 11, 9, 3, 7, 14, 3, 10, 10, 0, 5, 6, 0, 13],
    [15, 3, 1, 13, 8, 4, 14, 7, 6, 15, 11, 2, 3, 8, 4, 15, 9, 12, 7, 0, 2, 1, 13, 10, 12, 6, 0, 9, 5, 11, 10, 5, 0, 13, 14, 8, 7, 10, 11, 1, 10, 3, 4, 15, 13, 4, 1, 2, 5, 11, 8, 6, 12, 7, 6, 12, 9, 0, 3, 5, 2, 14, 15, 9],
    [10, 13, 0, 7, 9, 0, 14, 9, 6, 3, 3, 4, 15, 6, 5, 10, 1, 2, 13, 8, 12, 5, 7, 14, 11, 12, 4, 11, 2, 15, 8, 1, 13, 1, 6, 10, 4, 13, 9, 0, 8, 6, 15, 9, 3, 8, 0, 7, 11, 4, 1, 15, 2, 14, 12, 3, 5, 11, 10, 5, 14, 2, 7, 12],
    [7, 13, 13, 8, 14, 11, 3, 5, 0, 6, 6, 15, 9, 0, 10, 3, 1, 4, 2, 7, 8, 2, 5, 12, 11, 1, 12, 10, 4, 14, 15, 9, 10, 3, 6, 15, 9, 0, 0, 6, 12, 10, 11, 10, 7, 13, 13, 8, 15, 9, 1, 4, 3, 5, 14, 11, 5, 12, 2, 7, 8, 2, 4, 14],
    [2, 14, 12, 11, 4, 2, 1, 12, 7, 4, 10, 7, 11, 13, 6, 1, 8, 5, 5, 0, 3, 15, 15, 10, 13, 3, 0, 9, 14, 8, 9, 6, 4, 11, 2, 8, 1, 12, 11, 7, 10, 1, 13, 14, 7, 2, 8, 13, 15, 6, 9, 15, 12, 0, 5, 9, 6, 10, 3, 4, 0, 5, 14, 3],
    [12, 10, 1, 15, 10, 4, 15, 2, 9, 7, 2, 12, 6, 9, 8, 5, 0, 6, 13, 1, 3, 13, 4, 14, 14, 0, 7, 11, 5, 3, 11, 8, 9, 4, 14, 3, 15, 2, 5, 12, 2, 9, 8, 5, 12, 15, 3, 10, 7, 11, 0, 14, 4, 1, 10, 7, 1, 6, 13, 0, 11, 8, 6, 13],
    [4, 13, 11, 0, 2, 11, 14, 7, 15, 4, 0, 9, 8, 1, 13, 10, 3, 14, 12, 3, 9, 5, 7, 12, 5, 2, 10, 15, 6, 8, 1, 6, 1, 6, 4, 11, 11, 13, 13, 8, 12, 1, 3, 4, 7, 10, 14, 7, 10, 9, 15, 5, 6, 0, 8, 15, 0, 14, 5, 2, 9, 3, 2, 12],
    [13, 1, 2, 15, 8, 13, 4, 8, 6, 10, 15, 3, 11, 7, 1, 4, 10, 12, 9, 5, 3, 6, 14, 11, 5, 0, 0, 14, 12, 9, 7, 2, 7, 2, 11, 1, 4, 14, 1, 7, 9, 4, 12, 10, 14, 8, 2, 13, 0, 15, 6, 12, 10, 9, 13, 0, 15, 3, 3, 5, 5, 6, 8, 11],
];

/// The bit a table entry names: bits count down from the top of each 32-bit half, low half first.
fn bit(index: u8) -> u64 {
    let index = u32::from(index & 0x3f);
    if index < 32 { 1 << (31 - index) } else { 1 << (95 - index) }
}

fn map_bits(source: u64, table: &[u8]) -> u64 {
    table.iter().enumerate().fold(0, |out, (at, from)| if bit(*from) & source != 0 { out | bit(at as u8) } else { out })
}

fn map_u64(source: u64, table: &[u8]) -> u64 {
    let (low, high) = table.split_at(table.len() / 2);
    make_u64(map_bits(source, high) as u32, map_bits(source, low) as u32)
}

fn make_u64(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn rotate(param: &mut u32, left: u8) {
    *param = (*param << left) | ((*param >> (28 - left)) & 0xFFFF_FFF0);
}

fn subkeys(key: &[u8; 8], encrypt: bool) -> [u64; 16] {
    let param = map_u64(u64::from_le_bytes(*key), &KEY_PERMUTATION_TABLE);
    let (mut c, mut d) = (param as u32, (param >> 32) as u32);
    let mut keys = [0; 16];
    for (round, shift) in KEY_RND_SHIFTS.iter().enumerate() {
        rotate(&mut c, *shift);
        rotate(&mut d, *shift);
        keys[if encrypt { round } else { 15 - round }] = map_u64(make_u64(d, c), &KEY_COMPRESSION);
    }
    keys
}

fn round(state: u64, key: u64) -> u64 {
    let (high, low) = ((state >> 32) as u32, state as u32);
    let mixed = map_u64(make_u64(high, high), &KEY_EXPANSION) ^ key;
    let substituted = LARGE_STATE_SHIFTS.iter().zip(&SBOXES).fold(0u32, |out, (shift, sbox)| (out << 4) | u32::from(sbox[((mixed >> shift) & 0x3f) as usize]));
    make_u64(map_bits(u64::from(substituted), &P_BOX) as u32 ^ low, high)
}

fn transform(data: &mut [u8], keys: &[u64; 16]) {
    for block in data.chunks_exact_mut(8) {
        let value = u64::from_le_bytes(block.try_into().expect("eight bytes"));
        let state = keys.iter().fold(map_u64(value, &IP), |state, key| round(state, *key));
        block.copy_from_slice(&map_u64(state.rotate_left(32), &IP_INV).to_le_bytes());
    }
}

/// The hex text the lyric endpoint answers, as the QRC document inside it.
pub fn decode_hex(hex: &str) -> Option<String> {
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let mut data: Vec<u8> = (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok()).collect::<Option<_>>()?;
    if data.starts_with(&QMC_MAGIC) {
        for (at, byte) in data.iter_mut().enumerate() {
            *byte ^= QMC_KEY[(if at > 0x7FFF { at % 0x7FFF } else { at }) & 0x7F];
        }
        data.drain(..QMC_MAGIC.len());
    }
    if data.len() % 8 != 0 {
        return None;
    }
    for (key, encrypt) in KEYS {
        transform(&mut data, &subkeys(key, encrypt));
    }
    let mut text = String::new();
    flate2::read::ZlibDecoder::new(data.as_slice()).read_to_string(&mut text).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_comes_back_through_the_opposite_key_schedule() {
        let mut data = *b"mixtapes";
        transform(&mut data, &subkeys(b"!@#)(NHL", true));
        assert_ne!(&data, b"mixtapes");
        transform(&mut data, &subkeys(b"!@#)(NHL", false));
        assert_eq!(&data, b"mixtapes");
    }

    #[test]
    fn broken_input_is_nothing() {
        assert!(decode_hex("").is_none());
        assert!(decode_hex("abc").is_none());
        assert!(decode_hex("zz").is_none());
        assert!(decode_hex("00112233445566778899aabbccddeeff").is_none());
    }
}
