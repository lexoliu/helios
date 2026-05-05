extern crate alloc;

use crate::Ipv4Address;
use arrayvec::ArrayVec;

pub const DNS_PORT: u16 = 53;
pub const DNS_MAX_A_RECORDS: usize = 32;

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsMessage {
    pub id: u16,
    pub addresses: ArrayVec<Ipv4Address, DNS_MAX_A_RECORDS>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsResponse<'a> {
    bytes: &'a [u8],
}

impl<'a> DnsResponse<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 12 {
            return None;
        }
        let flags = read_u16(bytes, 2)?;
        if flags & 0x8000 == 0 || flags & 0x000f != 0 {
            return None;
        }
        Some(Self { bytes })
    }

    pub fn message(self) -> Option<DnsMessage> {
        let id = read_u16(self.bytes, 0)?;
        let question_count = read_u16(self.bytes, 4)?;
        let answer_count = read_u16(self.bytes, 6)?;
        let mut offset = 12usize;
        for _ in 0..question_count {
            offset = skip_name(self.bytes, offset)?;
            offset = offset.checked_add(4)?;
            if offset > self.bytes.len() {
                return None;
            }
        }

        let mut addresses = ArrayVec::new();
        for _ in 0..answer_count {
            offset = skip_name(self.bytes, offset)?;
            let record_type = read_u16(self.bytes, offset)?;
            let record_class = read_u16(self.bytes, offset + 2)?;
            let data_len = usize::from(read_u16(self.bytes, offset + 8)?);
            offset = offset.checked_add(10)?;
            let data_end = offset.checked_add(data_len)?;
            let data = self.bytes.get(offset..data_end)?;
            if record_type == TYPE_A && record_class == CLASS_IN && data.len() == 4 {
                addresses
                    .try_push(Ipv4Address::new(data.try_into().ok()?))
                    .ok()?;
            }
            offset = data_end;
        }

        Some(DnsMessage { id, addresses })
    }
}

pub struct DnsQuestionWriter<'a> {
    output: &'a mut [u8],
}

impl<'a> DnsQuestionWriter<'a> {
    pub fn new(output: &'a mut [u8]) -> Self {
        Self { output }
    }

    pub fn write_a_query(&mut self, id: u16, name: &str) -> Option<usize> {
        if self.output.len() < 12 {
            return None;
        }
        self.output[..12].fill(0);
        write_u16(self.output, 0, id)?;
        write_u16(self.output, 2, 0x0100)?;
        write_u16(self.output, 4, 1)?;
        let mut offset = 12usize;
        for label in name.split('.') {
            if label.is_empty() || label.len() > 63 {
                return None;
            }
            let end = offset.checked_add(1 + label.len())?;
            if self.output.len() < end {
                return None;
            }
            self.output[offset] = label.len() as u8;
            self.output[offset + 1..end].copy_from_slice(label.as_bytes());
            offset = end;
        }
        if self.output.len() < offset + 5 {
            return None;
        }
        self.output[offset] = 0;
        offset += 1;
        write_u16(self.output, offset, TYPE_A)?;
        offset += 2;
        write_u16(self.output, offset, CLASS_IN)?;
        offset += 2;
        Some(offset)
    }
}

fn skip_name(bytes: &[u8], mut offset: usize) -> Option<usize> {
    let mut jumps = 0usize;
    loop {
        let len = *bytes.get(offset)?;
        if len & 0xc0 == 0xc0 {
            let _pointer = read_u16(bytes, offset)? & 0x3fff;
            offset += 2;
            return Some(offset);
        }
        if len == 0 {
            return Some(offset + 1);
        }
        offset = offset.checked_add(usize::from(len) + 1)?;
        jumps += 1;
        if jumps > 128 {
            return None;
        }
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    Some(u16::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Option<()> {
    let end = offset.checked_add(2)?;
    bytes
        .get_mut(offset..end)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}
