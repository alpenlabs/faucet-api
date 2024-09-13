use crate::err;
use terrors::OneOf;

pub fn encode(bytes: &[u8]) -> String {
    let mut hex_string = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        hex_string.push(HexChar::try_from(b >> 4).expect("value should be 0..=15").into());
        hex_string.push(HexChar::try_from(b & 0b00001111).expect("value should be 0..=15").into());
    }
    hex_string
}

pub fn decode_alloc(hex: &str) -> Result<Vec<u8>, OneOf<(BadByte, UnevenHexCharacterCount)>> {
    let mut buf = vec![0u8; hex.len() / 2];
    decode(hex, &mut buf).map_err(|e| e.subset().expect("should not be WrongBufLength"))?;
    Ok(buf)
}

pub fn decode(
    hex: &str,
    buf: &mut [u8],
) -> Result<(), OneOf<(WrongBufLength, BadByte, UnevenHexCharacterCount)>> {
    let half_hex_len = hex.len() / 2;
    if half_hex_len * 2 != hex.len() {
        return err!(UnevenHexCharacterCount);
    }
    if buf.len() < half_hex_len {
        return err!(WrongBufLength {
            needed: half_hex_len,
            got: buf.len()
        });
    }

    let mut chars = hex.chars();

    let mut byte_count = 0;
    loop {
        let (c1, c2) = match (chars.next(), chars.next()) {
            (Some(c1), Some(c2)) => (c1, c2),
            (None, None) => break Ok(()),
            _ => break err!(BadByte { byte: byte_count })
        };

        let (Ok(c1), Ok(c2)) = (HexChar::try_from(c1), HexChar::try_from(c2)) else {
            for byte in buf {
                *byte = 0;
            }
            return err!(BadByte { byte: byte_count });
        };

        // first character is the first 4 bits so we calculate its
        // value then shift left by 4 bits.
        // we then calculate the second character by correcting its offset then
        // ORing the two together for the last 4 bits from c2
        buf[byte_count] = (<HexChar as Into<u8>>::into(c1) << 4) | <HexChar as Into<u8>>::into(c2);
        byte_count += 1;
    }
}

#[derive(Debug)]
pub struct WrongBufLength {
    pub needed: usize,
    pub got: usize,
}

#[derive(Debug)]
pub struct BadByte {
    byte: usize,
}

#[derive(Debug)]
pub struct UnevenHexCharacterCount;

enum HexCharKind {
    Uppercase,
    Lowercase,
    Number
}

impl TryFrom<&char> for HexCharKind {
    type Error = ();

    fn try_from(value: &char) -> Result<Self, Self::Error> {
        Ok(match value {
            '0'..='9' => Self::Number,
            'a'..='f' => Self::Lowercase,
            'A'..='F' => Self::Uppercase,
            _ => Err(())?
        })
    }
}

impl TryFrom<&u8> for HexCharKind {
    type Error = ();

    fn try_from(value: &u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0..=9 => Self::Number,
            10..=15 => Self::Lowercase,
            _ => Err(())?
        })
    }
}

impl HexCharKind {
    const fn offset(&self) -> u8 {
        match self {
            HexCharKind::Uppercase => 'A' as u8 - 0xA,
            HexCharKind::Lowercase => 'a' as u8 - 0xa,
            HexCharKind::Number => '0' as u8 - 0x0,
        }
    }
}

struct HexChar {
    c: char,
    kind: HexCharKind
}

impl TryFrom<char> for HexChar {
    type Error = ();

    fn try_from(value: char) -> Result<Self, Self::Error> {
        Ok(Self {
            kind: (&value).try_into()?,
            c: value
        })
    }
}

impl TryFrom<u8> for HexChar {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let kind = HexCharKind::try_from(&value)?;
        Ok(Self {
            c: (value + kind.offset()) as char,
            kind,
        })
    }
}

impl Into<u8> for HexChar {
    /// Converts the character to its [`u8`] equivalent. Eg, 'a' becomes 10.
    ///
    /// SAFETY: self is checked to be a valid hex character by [`HexChar`]'s
    /// [`TryFrom`] implementation.
    fn into(self) -> u8 {
        self.c as u8 - self.kind.offset()
    }
}

impl Into<char> for HexChar {
    fn into(self) -> char {
        self.c
    }
}

#[cfg(test)]
mod tests {
    use rand::{thread_rng, Rng};

    use super::*;

    #[test]
    fn hex_char_from_char() {
        for range in ['0'..='9', 'a'..='f', 'A'..='F'] {
            for c in range {
                assert!(HexChar::try_from(c).is_ok())
            }
        }
        for c in ['g', 'j', '.', '/'] {
            assert!(HexChar::try_from(c).is_err())
        }
    }

    #[test]
    fn hex_char_from_u8() {
        for u in 0u8..16 {
            assert!(HexChar::try_from(u).is_ok())
        }

        for u in 16u8..100 {
            assert!(HexChar::try_from(u).is_err())
        }
    }

    #[test]
    fn e2e() {
        let buf: [u8; 32] = thread_rng().gen();
        let string = encode(&buf);
        let mut debuf = [0u8; 32];
        assert!(decode(&string, &mut debuf).is_ok());
        assert_eq!(buf, debuf);

        let allocd = decode_alloc(&string);
        assert!(allocd.is_ok());
        if let Ok(vec) = allocd {
            assert_eq!(vec.len(), buf.len());
            for i in 0..buf.len() {
                assert_eq!(vec[i], buf[i])
            }
        }
    }
}
