//! The small binary encoding the sharing protocol uses: little endian integers, byte strings with
//! a u32 length, and hex for files and pairing codes.

pub type Res<T> = Result<T, &'static str>;

#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32).raw(v)
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
}

pub struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        R(b)
    }
    pub fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        if self.0.len() < n {
            return Err("truncated message");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    pub fn u8(&mut self) -> Res<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Res<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Res<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn array<const N: usize>(&mut self) -> Res<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    pub fn bytes(&mut self) -> Res<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn str(&mut self) -> Res<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| "not UTF-8")
    }
    /// A count that cannot be larger than what is left (each item takes at least `min` bytes), so
    /// a hostile count cannot make a `Vec` allocate more than the message is long.
    pub fn count(&mut self, min: usize) -> Res<usize> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min.max(1)) > self.0.len() {
            return Err("count larger than the message");
        }
        Ok(n)
    }
    /// Nothing left to read (a field a newer sender adds at the end may be absent from an older one's message).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn done(&self) -> Res<()> {
        if self.0.is_empty() { Ok(()) } else { Err("trailing bytes") }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_bounds() {
        let mut w = W::default();
        w.u8(7).u32(9).u64(1 << 40).str("DMM").bytes(&[1, 2, 3]);
        let mut r = R::new(&w.0);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u32().unwrap(), 9);
        assert_eq!(r.u64().unwrap(), 1 << 40);
        assert_eq!(r.str().unwrap(), "DMM");
        assert_eq!(r.bytes().unwrap(), &[1, 2, 3]);
        r.done().unwrap();
        assert!(R::new(&[5, 0, 0, 0, 1]).bytes().is_err(), "a length past the end");
        assert!(R::new(&[0xff, 0xff, 0xff, 0xff]).count(4).is_err(), "a count past the end");
        assert_eq!(unhex::<2>("0aFf"), Some([10, 255]));
        assert_eq!(unhex::<2>("0aF"), None);
        assert_eq!(hex(&[10, 255]), "0aff");
    }
}
