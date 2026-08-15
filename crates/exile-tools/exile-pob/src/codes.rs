//! Path of Building share codes: url-safe base64 over a zlib-compressed
//! build XML (the exact transform both engines' `ImportTab` uses, with
//! `-`/`_` in place of `+`/`/`). Decoding lives on the Rust side as the
//! primary path — headless engines stub their own `Inflate`/`Deflate`
//! to empty strings, so the code transform can never be delegated to
//! the engine (see `spikes/pob-headless/README.md`).

use std::io::Read;

use flate2::Compression;
use flate2::read::{ZlibDecoder, ZlibEncoder};

/// Decompressed-size ceiling: build XMLs run tens of KB; a code that
/// inflates past this is corrupt or hostile.
const MAX_XML_BYTES: u64 = 16 * 1024 * 1024;

/// Decode a share code into build XML.
pub fn decode(code: &str) -> Result<String, String> {
    let compressed = base64_decode(code.trim())?;
    let mut xml = String::new();
    ZlibDecoder::new(compressed.as_slice())
        .take(MAX_XML_BYTES)
        .read_to_string(&mut xml)
        .map_err(|err| format!("build code did not inflate to text: {err}"))?;
    if xml.is_empty() {
        return Err("build code inflated to nothing".to_owned());
    }
    Ok(xml)
}

/// Encode build XML as a share code.
pub fn encode(xml: &str) -> Result<String, String> {
    let mut compressed = Vec::new();
    ZlibEncoder::new(xml.as_bytes(), Compression::best())
        .read_to_end(&mut compressed)
        .map_err(|err| format!("deflate failed: {err}"))?;
    Ok(base64_encode(&compressed))
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for position in 0..4 {
            if position <= chunk.len() {
                let index = (bits >> (18 - 6 * position)) & 0x3f;
                let byte = ALPHABET[index as usize];
                // Share codes use the url-safe variant.
                out.push(match byte {
                    b'+' => '-',
                    b'/' => '_',
                    other => other as char,
                });
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(code: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(code.len() / 4 * 3);
    let mut bits: u32 = 0;
    let mut collected = 0u32;
    for ch in code.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            // Padding plus any whitespace a chat model may introduce
            // while copying a long code (wrapping, indentation).
            b'=' | b'\r' | b'\n' | b' ' | b'\t' => continue,
            other => {
                return Err(format!(
                    "invalid character in build code: {:?}",
                    other as char
                ));
            }
        };
        bits = (bits << 6) | u32::from(value);
        collected += 6;
        if collected >= 8 {
            collected -= 8;
            out.push(u8::try_from((bits >> collected) & 0xff).expect("masked to a byte"));
        }
    }
    if out.is_empty() {
        return Err("empty build code".to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_xml() {
        let xml = "<PathOfBuilding><Build level=\"90\"/></PathOfBuilding>";
        let code = encode(xml).expect("encodes");
        assert!(
            !code.contains('+') && !code.contains('/'),
            "url-safe: {code}"
        );
        assert_eq!(decode(&code).expect("decodes"), xml);
    }

    #[test]
    fn round_trips_binary_boundaries() {
        // Exercise 1/2/3-byte tail paddings through the base64 layer.
        for xml in ["a", "ab", "abc", "abcd", "<x>\u{e9}\u{2764}</x>"] {
            let code = encode(xml).expect("encodes");
            assert_eq!(decode(&code).expect("decodes"), xml, "input {xml:?}");
        }
    }

    #[test]
    fn accepts_standard_alphabet_and_padding() {
        // Codes copied from websites sometimes keep +/ and padding.
        let code = encode("<PathOfBuilding/>").expect("encodes");
        let standard: String = code
            .chars()
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                other => other,
            })
            .collect();
        assert_eq!(
            decode(&(standard + "==")).expect("decodes"),
            "<PathOfBuilding/>"
        );
    }

    #[test]
    fn tolerates_wrapped_codes() {
        // Chat models wrap long strings; whitespace must not break decode.
        let code = encode("<PathOfBuilding><Build/></PathOfBuilding>").expect("encodes");
        let wrapped: String = code
            .chars()
            .enumerate()
            .flat_map(|(index, c)| {
                if index == 10 {
                    vec!['\n', ' ', c]
                } else {
                    vec![c]
                }
            })
            .collect();
        assert_eq!(
            decode(&wrapped).expect("decodes"),
            "<PathOfBuilding><Build/></PathOfBuilding>"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("").is_err());
        assert!(decode("not a code!!").is_err());
        // Valid base64, not valid zlib.
        assert!(decode("aGVsbG8gd29ybGQ").is_err());
    }
}
