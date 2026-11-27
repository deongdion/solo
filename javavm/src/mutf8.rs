//! Modified UTF-8, the encoding JNI uses for `NewStringUTF` and `GetStringUTFChars`: U+0000 is
//! `C0 80`, and characters outside the BMP are two encoded surrogates (six bytes).

/// Encodes UTF-16 code units. Not NUL-terminated.
pub fn encode(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    for &u in units {
        match u {
            0x0001..=0x007f => out.push(u as u8),
            0x0000 | 0x0080..=0x07ff => {
                out.push(0xc0 | (u >> 6) as u8);
                out.push(0x80 | (u & 0x3f) as u8);
            }
            _ => {
                out.push(0xe0 | (u >> 12) as u8);
                out.push(0x80 | ((u >> 6) & 0x3f) as u8);
                out.push(0x80 | (u & 0x3f) as u8);
            }
        }
    }
    out
}

/// Decodes modified UTF-8 (and, leniently, standard UTF-8 4-byte sequences, which native code
/// often passes). Invalid bytes become U+FFFD, as ART's CheckJNI-off path does not stop on them.
pub fn decode(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let cont = |b: Option<&u8>| b.filter(|&&b| b & 0xc0 == 0x80).map(|&b| (b & 0x3f) as u32);
    while i < bytes.len() {
        let b = bytes[i] as u32;
        if b < 0x80 {
            out.push(b as u16);
            i += 1;
        } else if b & 0xe0 == 0xc0 {
            match cont(bytes.get(i + 1)) {
                Some(c) => {
                    out.push((((b & 0x1f) << 6) | c) as u16);
                    i += 2;
                }
                None => {
                    out.push(0xfffd);
                    i += 1;
                }
            }
        } else if b & 0xf0 == 0xe0 {
            match (cont(bytes.get(i + 1)), cont(bytes.get(i + 2))) {
                (Some(c1), Some(c2)) => {
                    out.push((((b & 0x0f) << 12) | (c1 << 6) | c2) as u16);
                    i += 3;
                }
                _ => {
                    out.push(0xfffd);
                    i += 1;
                }
            }
        } else if b & 0xf8 == 0xf0 {
            match (cont(bytes.get(i + 1)), cont(bytes.get(i + 2)), cont(bytes.get(i + 3))) {
                (Some(c1), Some(c2), Some(c3)) => {
                    let cp = ((b & 0x07) << 18) | (c1 << 12) | (c2 << 6) | c3;
                    if (0x10000..=0x10ffff).contains(&cp) {
                        let v = cp - 0x10000;
                        out.push(0xd800 | (v >> 10) as u16);
                        out.push(0xdc00 | (v & 0x3ff) as u16);
                    } else {
                        out.push(0xfffd);
                    }
                    i += 4;
                }
                _ => {
                    out.push(0xfffd);
                    i += 1;
                }
            }
        } else {
            out.push(0xfffd);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_bmp_nul_and_supplementary() {
        let text: Vec<u16> = "a\u{0}é한😀".encode_utf16().collect();
        let encoded = encode(&text);
        assert_eq!(&encoded[..3], &[b'a', 0xc0, 0x80]);
        assert!(!encoded.contains(&0));
        // The emoji is two 3-byte surrogates, not one 4-byte sequence.
        assert_eq!(encoded.len(), 1 + 2 + 2 + 3 + 6);
        assert_eq!(decode(&encoded), text);
    }

    #[test]
    fn accepts_standard_utf8_and_replaces_garbage() {
        let text: Vec<u16> = "😀".encode_utf16().collect();
        assert_eq!(decode("😀".as_bytes()), text);
        assert_eq!(decode(&[b'x', 0xff, 0xc3]), vec![b'x' as u16, 0xfffd, 0xfffd]);
    }
}
