//! XDR (RFC 4506): big-endian, padded to four bytes. Only the shapes ONC RPC and VXI-11 use.

/// A cursor over a received message. Every read is bounds-checked and a short buffer is `None`,
/// never a panic: this parses what came off a socket.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub fn u32(&mut self) -> Option<u32> {
        let b = self.buf.get(self.pos..self.pos.checked_add(4)?)?;
        self.pos += 4;
        Some(u32::from_be_bytes(b.try_into().ok()?))
    }

    /// Variable-length opaque: a length, the bytes, padding to four. `max` caps the declared length
    /// before anything is sliced: four bytes can claim 4 GiB.
    pub fn opaque(&mut self, max: usize) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > max {
            return None;
        }
        let end = self.pos.checked_add(len)?;
        let out = self.buf.get(self.pos..end)?;
        // A sender that leaves off the last field's padding has still sent all of it.
        self.pos = ((end + 3) & !3).min(self.buf.len());
        Some(out)
    }
}

/// An outgoing message.
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u32(v as u32)
    }

    pub fn opaque(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.buf.extend_from_slice(b);
        self.buf.extend_from_slice(&[0u8; 3][..(4 - b.len() % 4) % 4]);
        self
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_is_padded_and_read_back() {
        let mut e = Encoder::new();
        e.opaque(b"hello").u32(7);
        let b = e.into_vec();
        assert_eq!(b.len(), 4 + 8 + 4);
        let mut d = Decoder::new(&b);
        assert_eq!(d.opaque(64), Some(&b"hello"[..]));
        assert_eq!(d.u32(), Some(7));
        assert_eq!(d.u32(), None);
    }

    #[test]
    fn a_length_past_the_cap_or_the_buffer_is_refused() {
        assert_eq!(Decoder::new(&[0xFF; 4]).opaque(1024), None);
        assert_eq!(Decoder::new(&[0, 0, 0, 16, 1, 2, 3, 4]).opaque(1024), None);
        // No padding after the last field: still the whole payload.
        assert_eq!(Decoder::new(&[0, 0, 0, 1, 0xAB]).opaque(16), Some(&[0xABu8][..]));
    }
}
