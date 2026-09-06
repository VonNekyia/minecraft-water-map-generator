//! A minimal, allocation-free, streaming NBT reader.
//!
//! The scanner only ever needs a handful of fields out of a chunk, so instead of
//! materialising a full NBT tree we walk the byte stream and skip everything we
//! are not interested in. Strings and array payloads are returned as borrowed
//! slices into the decompressed chunk buffer, so parsing a chunk performs zero
//! heap allocations.

use std::fmt;

pub const TAG_END: u8 = 0;
pub const TAG_BYTE: u8 = 1;
pub const TAG_SHORT: u8 = 2;
pub const TAG_INT: u8 = 3;
pub const TAG_LONG: u8 = 4;
pub const TAG_FLOAT: u8 = 5;
pub const TAG_DOUBLE: u8 = 6;
pub const TAG_BYTE_ARRAY: u8 = 7;
pub const TAG_STRING: u8 = 8;
pub const TAG_LIST: u8 = 9;
pub const TAG_COMPOUND: u8 = 10;
pub const TAG_INT_ARRAY: u8 = 11;
pub const TAG_LONG_ARRAY: u8 = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NbtError {
    Eof,
    BadTag(u8),
    BadUtf8,
    NegativeLength,
    TooDeep,
}

impl fmt::Display for NbtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NbtError::Eof => write!(f, "unexpected end of NBT data"),
            NbtError::BadTag(t) => write!(f, "unknown NBT tag id {t}"),
            NbtError::BadUtf8 => write!(f, "invalid UTF-8 in NBT string"),
            NbtError::NegativeLength => write!(f, "negative NBT array length"),
            NbtError::TooDeep => write!(f, "NBT nesting too deep"),
        }
    }
}

impl std::error::Error for NbtError {}

pub type Result<T> = std::result::Result<T, NbtError>;

/// A borrowed big-endian `long[]` payload.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct LongArray<'a> {
    bytes: &'a [u8],
}

#[allow(dead_code)]
impl<'a> LongArray<'a> {
    pub const EMPTY: LongArray<'static> = LongArray { bytes: &[] };

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len() / 8
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() < 8
    }

    /// Reads element `i`. Returns 0 when out of range so callers stay branch-light
    /// on malformed data instead of panicking mid-scan.
    #[inline]
    pub fn get(&self, i: usize) -> i64 {
        let o = i * 8;
        if o + 8 > self.bytes.len() {
            return 0;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.bytes[o..o + 8]);
        i64::from_be_bytes(b)
    }
}

pub struct NbtReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[allow(dead_code)]
impl<'a> NbtReader<'a> {
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(NbtError::Eof)?;
        if end > self.buf.len() {
            return Err(NbtError::Eof);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        if self.pos >= self.buf.len() {
            return Err(NbtError::Eof);
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    #[inline]
    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }

    #[inline]
    pub fn i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        Ok(self.i16()? as u16)
    }

    #[inline]
    pub fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    #[inline]
    pub fn i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    #[inline]
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.i32()? as u32))
    }

    #[inline]
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.i64()? as u64))
    }

    /// Reads a length-prefixed modified-UTF-8 string.
    ///
    /// Minecraft only emits plain ASCII/UTF-8 for the keys and resource locations
    /// we care about, so plain UTF-8 validation is sufficient here.
    #[inline]
    pub fn string(&mut self) -> Result<&'a str> {
        let n = self.u16()? as usize;
        let b = self.take(n)?;
        std::str::from_utf8(b).map_err(|_| NbtError::BadUtf8)
    }

    #[inline]
    pub fn skip_string(&mut self) -> Result<()> {
        let n = self.u16()? as usize;
        self.take(n)?;
        Ok(())
    }

    #[inline]
    fn array_len(&mut self) -> Result<usize> {
        let n = self.i32()?;
        if n < 0 {
            return Err(NbtError::NegativeLength);
        }
        Ok(n as usize)
    }

    #[inline]
    pub fn byte_array(&mut self) -> Result<&'a [u8]> {
        let n = self.array_len()?;
        self.take(n)
    }

    #[inline]
    pub fn long_array(&mut self) -> Result<LongArray<'a>> {
        let n = self.array_len()?;
        let bytes = self.take(n.checked_mul(8).ok_or(NbtError::Eof)?)?;
        Ok(LongArray { bytes })
    }

    /// Reads a list header, returning `(element_tag, length)`.
    #[inline]
    pub fn list_header(&mut self) -> Result<(u8, usize)> {
        let tag = self.u8()?;
        let n = self.array_len()?;
        Ok((tag, n))
    }

    /// Skips the payload of a value of type `tag`. The tag id and any name must
    /// already have been consumed.
    pub fn skip_payload(&mut self, tag: u8) -> Result<()> {
        self.skip_payload_depth(tag, 0)
    }

    fn skip_payload_depth(&mut self, tag: u8, depth: u32) -> Result<()> {
        if depth > 512 {
            return Err(NbtError::TooDeep);
        }
        match tag {
            TAG_END => Ok(()),
            TAG_BYTE => {
                self.take(1)?;
                Ok(())
            }
            TAG_SHORT => {
                self.take(2)?;
                Ok(())
            }
            TAG_INT | TAG_FLOAT => {
                self.take(4)?;
                Ok(())
            }
            TAG_LONG | TAG_DOUBLE => {
                self.take(8)?;
                Ok(())
            }
            TAG_BYTE_ARRAY => {
                let n = self.array_len()?;
                self.take(n)?;
                Ok(())
            }
            TAG_STRING => self.skip_string(),
            TAG_INT_ARRAY => {
                let n = self.array_len()?;
                self.take(n.checked_mul(4).ok_or(NbtError::Eof)?)?;
                Ok(())
            }
            TAG_LONG_ARRAY => {
                let n = self.array_len()?;
                self.take(n.checked_mul(8).ok_or(NbtError::Eof)?)?;
                Ok(())
            }
            TAG_LIST => {
                let (etag, n) = self.list_header()?;
                // Fixed-width element types can be skipped with a single jump.
                let width = match etag {
                    TAG_BYTE => 1,
                    TAG_SHORT => 2,
                    TAG_INT | TAG_FLOAT => 4,
                    TAG_LONG | TAG_DOUBLE => 8,
                    _ => 0,
                };
                if width > 0 {
                    self.take(n.checked_mul(width).ok_or(NbtError::Eof)?)?;
                    return Ok(());
                }
                for _ in 0..n {
                    self.skip_payload_depth(etag, depth + 1)?;
                }
                Ok(())
            }
            TAG_COMPOUND => loop {
                let t = self.u8()?;
                if t == TAG_END {
                    return Ok(());
                }
                self.skip_string()?;
                self.skip_payload_depth(t, depth + 1)?;
            },
            other => Err(NbtError::BadTag(other)),
        }
    }

    /// Consumes the root header (`TAG_Compound` + name) of a chunk NBT stream.
    pub fn open_root(&mut self) -> Result<()> {
        let t = self.u8()?;
        if t != TAG_COMPOUND {
            return Err(NbtError::BadTag(t));
        }
        self.skip_string()?;
        Ok(())
    }

    /// Iterates the entries of the compound currently being read. Returns `None`
    /// once `TAG_End` is reached.
    #[inline]
    pub fn next_entry(&mut self) -> Result<Option<(u8, &'a str)>> {
        let t = self.u8()?;
        if t == TAG_END {
            return Ok(None);
        }
        let name = self.string()?;
        Ok(Some((t, name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds `{ "a": 1i32, "b": "hi", "c": [1i64, 2i64] }` by hand.
    fn sample() -> Vec<u8> {
        let mut v = Vec::new();
        v.push(TAG_COMPOUND);
        v.extend_from_slice(&0u16.to_be_bytes());
        v.push(TAG_INT);
        v.extend_from_slice(&1u16.to_be_bytes());
        v.push(97);
        v.extend_from_slice(&1i32.to_be_bytes());
        v.push(TAG_STRING);
        v.extend_from_slice(&1u16.to_be_bytes());
        v.push(98);
        v.extend_from_slice(&2u16.to_be_bytes());
        v.extend_from_slice("hi".as_bytes());
        v.push(TAG_LONG_ARRAY);
        v.extend_from_slice(&1u16.to_be_bytes());
        v.push(99);
        v.extend_from_slice(&2i32.to_be_bytes());
        v.extend_from_slice(&1i64.to_be_bytes());
        v.extend_from_slice(&2i64.to_be_bytes());
        v.push(TAG_END);
        v
    }

    #[test]
    fn reads_and_skips_entries() {
        let data = sample();
        let mut r = NbtReader::new(&data);
        r.open_root().unwrap();

        let (t, n) = r.next_entry().unwrap().unwrap();
        assert_eq!((t, n), (TAG_INT, "a"));
        assert_eq!(r.i32().unwrap(), 1);

        let (t, n) = r.next_entry().unwrap().unwrap();
        assert_eq!((t, n), (TAG_STRING, "b"));
        assert_eq!(r.string().unwrap(), "hi");

        let (t, n) = r.next_entry().unwrap().unwrap();
        assert_eq!((t, n), (TAG_LONG_ARRAY, "c"));
        let la = r.long_array().unwrap();
        assert_eq!(la.len(), 2);
        assert_eq!(la.get(0), 1);
        assert_eq!(la.get(1), 2);

        assert!(r.next_entry().unwrap().is_none());
    }

    #[test]
    fn skip_payload_walks_over_values() {
        let data = sample();
        let mut r = NbtReader::new(&data);
        r.open_root().unwrap();
        while let Some((t, _)) = r.next_entry().unwrap() {
            r.skip_payload(t).unwrap();
        }
        assert_eq!(r.position(), data.len());
    }

    #[test]
    fn truncated_input_errors_instead_of_panicking() {
        let data = sample();
        for cut in 1..data.len() {
            let mut r = NbtReader::new(&data[..cut]);
            let _ = (|| -> Result<()> {
                r.open_root()?;
                while let Some((t, _)) = r.next_entry()? {
                    r.skip_payload(t)?;
                }
                Ok(())
            })();
        }
    }

    #[test]
    fn long_array_out_of_range_returns_zero() {
        let bytes = [0u8; 8];
        let la = LongArray { bytes: &bytes };
        assert_eq!(la.get(0), 0);
        assert_eq!(la.get(5), 0);
    }
}
