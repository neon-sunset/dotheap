use super::{CaptureError, Result};

#[derive(Clone, Debug)]
pub(super) struct Mapping {
    pub start: u64,
    pub end: u64,
    pub read: bool,
    pub write: bool,
    pub execute: bool,
    pub shared: bool,
    pub offset: u64,
    pub path: String,
}

impl Mapping {
    pub fn pages(&self, page_size: usize) -> usize {
        ((self.end - self.start) / page_size as u64) as usize
    }
    pub fn file_backed(&self) -> bool {
        self.path.starts_with('/')
    }
    pub fn anonymous(&self) -> bool {
        !self.shared && !self.file_backed() && self.path != "[vdso]"
    }
    pub fn flags(&self) -> u32 {
        u32::from(self.read) * 4 | u32::from(self.write) * 2 | u32::from(self.execute)
    }

    pub fn parse(bytes: &[u8], page_size: usize) -> Result<Vec<Self>> {
        let text = String::from_utf8_lossy(bytes);
        let mut mappings: Vec<Self> = Vec::new();
        for line in text.lines().filter(|s| !s.is_empty()) {
            let invalid = || CaptureError::terminal("/proc maps contains an invalid memory range");
            let mut rest = line;
            let mut fields = Vec::with_capacity(5);
            for _ in 0..5 {
                rest = rest.trim_start();
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                fields.push(&rest[..end]);
                rest = &rest[end..];
            }
            let (start, end) = fields[0].split_once('-').ok_or_else(invalid)?;
            let start = u64::from_str_radix(start, 16).map_err(|_| invalid())?;
            let end = u64::from_str_radix(end, 16).map_err(|_| invalid())?;
            let offset = u64::from_str_radix(fields[2], 16).map_err(|_| invalid())?;
            let perms = fields[1].as_bytes();
            if start >= end
                || start % page_size as u64 != 0
                || end % page_size as u64 != 0
                || perms.len() != 4
                || !b"r-".contains(&perms[0])
                || !b"w-".contains(&perms[1])
                || !b"x-".contains(&perms[2])
                || !b"ps".contains(&perms[3])
            {
                return Err(invalid());
            }
            if mappings.last().is_some_and(|m| m.end > start) {
                return Err(invalid());
            }
            let path = rest.trim_start().to_string();
            let mapping = Self {
                start,
                end,
                offset,
                path,
                read: perms[0] == b'r',
                write: perms[1] == b'w',
                execute: perms[2] == b'x',
                shared: perms[3] == b's',
            };
            if mapping.flags() == 0
                || mapping.path.starts_with("[vvar")
                || mapping.path == "[vsyscall]"
            {
                continue;
            }
            mappings.push(mapping);
        }
        if mappings.is_empty() {
            return Err(CaptureError::terminal(
                "the process exited or has no readable mappings",
            ));
        }
        Ok(mappings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mappings_preserve_deleted_paths_and_shared_permissions() {
        let text = b"1000-2000 r--s 00001000 08:01 12    /tmp/a b (deleted)\n2000-3000 rw-p 00000000 00:00 0\n3000-4000 r--p 00000000 00:00 0 [vvar_vclock]\n4000-5000 ---p 00000000 00:00 0\n";
        let mappings = Mapping::parse(text, 4096).unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].path, "/tmp/a b (deleted)");
        assert_eq!(mappings[0].offset, 4096);
        assert!(mappings[0].shared);
        assert!(!mappings[0].anonymous());
        assert!(mappings[1].anonymous());
        for invalid in [
            "1000-2000 r 0 00:00 0",
            "2000-1000 r--p 0 00:00 0",
            "1001-2000 r--p 0 00:00 0",
        ] {
            assert!(Mapping::parse(invalid.as_bytes(), 4096).is_err());
        }
    }
}
