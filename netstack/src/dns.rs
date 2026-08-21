extern crate alloc;

use crate::{IpAddress, Ipv4Address, Ipv6Address};
use arrayvec::ArrayVec;

pub const DNS_PORT: u16 = 53;
/// Maximum address records collected from one response, across both
/// address families.
pub const DNS_MAX_ADDRESS_RECORDS: usize = 32;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

/// Which address family a question asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsRecordType {
    /// `A`: IPv4 host address.
    A,
    /// `AAAA`: IPv6 host address.
    Aaaa,
}

impl DnsRecordType {
    const fn code(self) -> u16 {
        match self {
            Self::A => TYPE_A,
            Self::Aaaa => TYPE_AAAA,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsMessage {
    pub id: u16,
    /// Every `A` and `AAAA` record in the answer section, in the order
    /// the server returned them. Both families land in one list because
    /// `wasi:sockets/ip-name-lookup` hands the guest a single address
    /// stream with no ordering guarantee.
    pub addresses: ArrayVec<IpAddress, DNS_MAX_ADDRESS_RECORDS>,
}

impl DnsMessage {
    pub fn ipv4_addresses(&self) -> impl Iterator<Item = Ipv4Address> + '_ {
        self.addresses.iter().filter_map(|address| match address {
            IpAddress::Ipv4(address) => Some(*address),
            IpAddress::Ipv6(_) => None,
        })
    }

    pub fn ipv6_addresses(&self) -> impl Iterator<Item = Ipv6Address> + '_ {
        self.addresses.iter().filter_map(|address| match address {
            IpAddress::Ipv6(address) => Some(*address),
            IpAddress::Ipv4(_) => None,
        })
    }
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
            if record_class == CLASS_IN {
                match (record_type, data.len()) {
                    (TYPE_A, 4) => {
                        let _ = addresses
                            .try_push(IpAddress::Ipv4(Ipv4Address::new(data.try_into().ok()?)));
                    }
                    (TYPE_AAAA, 16) => {
                        let _ = addresses
                            .try_push(IpAddress::Ipv6(Ipv6Address::new(data.try_into().ok()?)));
                    }
                    _ => {}
                }
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

    /// Writes an `A` question. Kept as a named entry point because the
    /// IPv4-only paths (DHCP-configured resolvers, ICMP echo target
    /// resolution) ask for nothing else.
    pub fn write_a_query(&mut self, id: u16, name: &str) -> Option<usize> {
        self.write_query(id, name, DnsRecordType::A)
    }

    /// Writes an `AAAA` question.
    pub fn write_aaaa_query(&mut self, id: u16, name: &str) -> Option<usize> {
        self.write_query(id, name, DnsRecordType::Aaaa)
    }

    pub fn write_query(&mut self, id: u16, name: &str, record: DnsRecordType) -> Option<usize> {
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
        write_u16(self.output, offset, record.code())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes an answer record for `example.com` using a compression
    /// pointer to the question's name, the way real resolvers do.
    fn push_answer(bytes: &mut alloc::vec::Vec<u8>, record_type: u16, data: &[u8]) {
        bytes.extend_from_slice(&[0xc0, 0x0c]);
        bytes.extend_from_slice(&record_type.to_be_bytes());
        bytes.extend_from_slice(&CLASS_IN.to_be_bytes());
        bytes.extend_from_slice(&60u32.to_be_bytes());
        bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
        bytes.extend_from_slice(data);
    }

    fn response_with(answers: &[(u16, &[u8])]) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        bytes.extend_from_slice(&0x1234u16.to_be_bytes());
        bytes.extend_from_slice(&0x8180u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        for label in ["example", "com"] {
            bytes.push(label.len() as u8);
            bytes.extend_from_slice(label.as_bytes());
        }
        bytes.push(0);
        bytes.extend_from_slice(&TYPE_A.to_be_bytes());
        bytes.extend_from_slice(&CLASS_IN.to_be_bytes());
        for (record_type, data) in answers {
            push_answer(&mut bytes, *record_type, data);
        }
        bytes
    }

    #[test]
    fn aaaa_query_differs_from_a_query_only_in_the_question_type() {
        let mut a = [0u8; 64];
        let mut aaaa = [0u8; 64];
        let a_len = DnsQuestionWriter::new(&mut a)
            .write_a_query(7, "example.com")
            .expect("A query should encode");
        let aaaa_len = DnsQuestionWriter::new(&mut aaaa)
            .write_aaaa_query(7, "example.com")
            .expect("AAAA query should encode");

        assert_eq!(a_len, aaaa_len);
        assert_eq!(&a[..a_len - 4], &aaaa[..aaaa_len - 4]);
        assert_eq!(
            u16::from_be_bytes([a[a_len - 4], a[a_len - 3]]),
            TYPE_A
        );
        assert_eq!(
            u16::from_be_bytes([aaaa[aaaa_len - 4], aaaa[aaaa_len - 3]]),
            TYPE_AAAA
        );
        assert_eq!(
            u16::from_be_bytes([aaaa[aaaa_len - 2], aaaa[aaaa_len - 1]]),
            CLASS_IN
        );
    }

    #[test]
    fn response_collects_both_address_families() {
        let bytes = response_with(&[
            (TYPE_A, &[93, 184, 215, 14]),
            (
                TYPE_AAAA,
                &[
                    0x26, 0x06, 0x28, 0x00, 0x02, 0x1f, 0xcb, 0x07, 0x68, 0x20, 0x80, 0xda, 0x00,
                    0xaf, 0x6b, 0x08,
                ],
            ),
        ]);
        let message = DnsResponse::parse(&bytes)
            .and_then(DnsResponse::message)
            .expect("response should parse");

        assert_eq!(message.id, 0x1234);
        assert_eq!(message.addresses.len(), 2);
        assert_eq!(
            message.ipv4_addresses().collect::<alloc::vec::Vec<_>>(),
            alloc::vec![Ipv4Address::new([93, 184, 215, 14])]
        );
        assert_eq!(
            message.ipv6_addresses().collect::<alloc::vec::Vec<_>>(),
            alloc::vec![Ipv6Address::new([
                0x26, 0x06, 0x28, 0x00, 0x02, 0x1f, 0xcb, 0x07, 0x68, 0x20, 0x80, 0xda, 0x00, 0xaf,
                0x6b, 0x08,
            ])]
        );
    }

    #[test]
    fn malformed_address_records_are_skipped_without_failing_the_response() {
        // A CNAME and an A record whose RDATA length lies about the
        // family must not stop the AAAA record behind them from parsing.
        let bytes = response_with(&[
            (5, &[0xc0, 0x0c]),
            (TYPE_A, &[1, 2, 3]),
            (
                TYPE_AAAA,
                &[0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3],
            ),
        ]);
        let message = DnsResponse::parse(&bytes)
            .and_then(DnsResponse::message)
            .expect("response should parse");

        assert_eq!(
            message.addresses.as_slice(),
            &[IpAddress::Ipv6(Ipv6Address::new([
                0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3
            ]))]
        );
    }

    #[test]
    fn responses_beyond_the_record_cap_truncate_instead_of_failing() {
        let addresses: alloc::vec::Vec<(u16, &[u8])> = (0..DNS_MAX_ADDRESS_RECORDS + 4)
            .map(|_| (TYPE_A, &[10, 0, 0, 1][..]))
            .collect();
        let bytes = response_with(&addresses);
        let message = DnsResponse::parse(&bytes)
            .and_then(DnsResponse::message)
            .expect("oversized response should still parse");

        assert_eq!(message.addresses.len(), DNS_MAX_ADDRESS_RECORDS);
    }
}
