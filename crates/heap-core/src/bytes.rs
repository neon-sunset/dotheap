pub trait ReadBytes {
    fn u8(&self, at: usize) -> Option<u8>;
    fn u16(&self, at: usize) -> Option<u16>;
    fn u32(&self, at: usize) -> Option<u32>;
    fn u64(&self, at: usize) -> Option<u64>;
    fn c_string(&self, at: usize, limit: usize) -> Option<String>;
}

impl ReadBytes for [u8] {
    #[inline]
    fn u8(&self, at: usize) -> Option<u8> {
        self.get(at).copied()
    }
    #[inline]
    fn u16(&self, at: usize) -> Option<u16> {
        Some(u16::from_le_bytes(
            self.get(at..at.checked_add(2)?)?.try_into().ok()?,
        ))
    }
    #[inline]
    fn u32(&self, at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(
            self.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    }
    #[inline]
    fn u64(&self, at: usize) -> Option<u64> {
        Some(u64::from_le_bytes(
            self.get(at..at.checked_add(8)?)?.try_into().ok()?,
        ))
    }
    fn c_string(&self, at: usize, limit: usize) -> Option<String> {
        let bytes = self.get(at..self.len().min(at.saturating_add(limit)))?;
        let n = memchr::memchr(0, bytes)?;
        bytes[..n]
            .iter()
            .all(|b| (0x20..=0x7e).contains(b))
            .then(|| String::from_utf8_lossy(&bytes[..n]).into_owned())
    }
}

pub fn align_up(value: u64, alignment: u64) -> Option<u64> {
    if !alignment.is_power_of_two() {
        return None;
    }
    Some(value.checked_add(alignment - 1)? & !(alignment - 1))
}
