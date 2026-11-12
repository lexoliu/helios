extern crate alloc;

use crate::{IpAddress, Ipv4Address, Ipv6Address};
use arrayvec::ArrayVec;

pub const DNS_PORT: u16 = 53;
/// Maximum address records collected from one response, across both
/// address families.
pub const DNS_MAX_ADDRESS_RECORDS: usize = 32;

/// `CNAME` links one response may chain from the queried name: Unbound's
/// default `max-query-restarts`, the limit it puts on the chain it will
/// follow. A longer chain is not followed and its far end is not trusted.
pub const DNS_MAX_CNAME_CHAIN: usize = 11;

const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;
/// The first question starts right after the 12-byte header (RFC 1035
/// section 4.1.1).
const QUESTION_OFFSET: usize = 12;
/// A name's wire form, length octets and root label included, is at most
/// 255 octets (RFC 1035 section 2.3.4).
const MAX_NAME_OCTETS: usize = 255;

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

/// The question a response has to answer to be believed.
///
/// RFC 5452 section 9.1 and RFC 1035 section 7.3: a response is the
/// answer to a query only when its question section repeats the query's,
/// and only the records that question reaches are its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsQuery<'a> {
    /// The queried name, as written to [`DnsQuestionWriter`]: dotted
    /// labels without a trailing dot.
    pub name: &'a str,
    pub record: DnsRecordType,
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

    /// The transaction id, which selects the outstanding query this
    /// response claims to answer.
    pub fn id(self) -> u16 {
        // `parse` checked the header is present.
        u16::from_be_bytes([self.bytes[0], self.bytes[1]])
    }

    /// The addresses this response gives for `query`, or `None` when it
    /// is not an answer to it.
    ///
    /// The question section must hold exactly one question whose name
    /// (compared case-insensitively, RFC 1035 section 2.3.3), type and
    /// class equal the query's. Only records of the queried type owned by
    /// the queried name, or by a name its `CNAME` chain reaches, are
    /// collected; records the forger had no reason to know the query for
    /// are not.
    pub fn message(self, query: DnsQuery<'_>) -> Option<DnsMessage> {
        let bytes = self.bytes;
        let id = self.id();
        let question_count = read_u16(bytes, 4)?;
        let answer_count = read_u16(bytes, 6)?;
        if question_count != 1 {
            return None;
        }
        let question_name = QUESTION_OFFSET;
        let mut offset = skip_name(bytes, question_name)?;
        let question_type = read_u16(bytes, offset)?;
        let question_class = read_u16(bytes, offset.checked_add(2)?)?;
        offset = offset.checked_add(4)?;
        if question_type != query.record.code()
            || question_class != CLASS_IN
            || !name_equals_text(bytes, question_name, query.name)?
        {
            return None;
        }
        let answers_start = offset;

        // Offsets of the names whose records answer the question: the
        // question's own, then each `CNAME` target the chain reaches from
        // it. A name with a `CNAME` has no other data (RFC 2181 section
        // 10.1), so the chain is a path: each step looks for the `CNAME`
        // owned by its far end, wherever the response lists it, and the
        // walk stops at a name with none, at a name already on the path,
        // or at the chain limit.
        let mut owners: ArrayVec<usize, { DNS_MAX_CNAME_CHAIN + 1 }> = ArrayVec::new();
        owners.push(question_name);
        while !owners.is_full() {
            let end = owners[owners.len() - 1];
            let mut target = None;
            let mut cursor = answers_start;
            for _ in 0..answer_count {
                let record = read_record(bytes, &mut cursor)?;
                if record.class == CLASS_IN
                    && record.record_type == TYPE_CNAME
                    && names_equal(bytes, end, record.name)?
                {
                    target = Some(record.data_offset);
                    break;
                }
            }
            let Some(target) = target else {
                break;
            };
            if is_owner(bytes, &owners, target)? {
                break;
            }
            owners.push(target);
        }

        let mut addresses = ArrayVec::new();
        let mut cursor = answers_start;
        for _ in 0..answer_count {
            let record = read_record(bytes, &mut cursor)?;
            if record.class != CLASS_IN
                || record.record_type != query.record.code()
                || !is_owner(bytes, &owners, record.name)?
            {
                continue;
            }
            match (record.record_type, record.data) {
                (TYPE_A, data) if data.len() == 4 => {
                    let _ = addresses
                        .try_push(IpAddress::Ipv4(Ipv4Address::new(data.try_into().ok()?)));
                }
                (TYPE_AAAA, data) if data.len() == 16 => {
                    let _ = addresses
                        .try_push(IpAddress::Ipv6(Ipv6Address::new(data.try_into().ok()?)));
                }
                _ => {}
            }
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
        // The labels and the root octet below are the name's wire form.
        if offset - QUESTION_OFFSET + 1 > MAX_NAME_OCTETS {
            return None;
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

/// Where the name at `offset` ends in the message: past its root label or
/// its first compression pointer. `None` when the name does not decode.
fn skip_name(bytes: &[u8], offset: usize) -> Option<usize> {
    let mut labels = Labels::new(bytes, offset);
    for label in labels.by_ref() {
        label.ok()?;
    }
    labels.end_in_place
}

/// One resource record, located in the message rather than copied out of
/// it: its owner name and its `RDATA` are offsets or slices of the
/// datagram.
struct Record<'a> {
    /// Offset of the owner name.
    name: usize,
    record_type: u16,
    class: u16,
    /// Offset of the `RDATA`, which for a `CNAME` is the target name.
    data_offset: usize,
    data: &'a [u8],
}

/// Reads the record at `*cursor` and moves the cursor past it.
fn read_record<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<Record<'a>> {
    let name = *cursor;
    let mut offset = skip_name(bytes, name)?;
    let record_type = read_u16(bytes, offset)?;
    let class = read_u16(bytes, offset.checked_add(2)?)?;
    let data_len = usize::from(read_u16(bytes, offset.checked_add(8)?)?);
    offset = offset.checked_add(10)?;
    let data_end = offset.checked_add(data_len)?;
    let data = bytes.get(offset..data_end)?;
    *cursor = data_end;
    Some(Record {
        name,
        record_type,
        class,
        data_offset: offset,
        data,
    })
}

/// A name that does not decode: a label running past the message, a
/// reserved label type, a compression pointer that does not point back
/// past the last one, or more than [`MAX_NAME_OCTETS`].
struct MalformedName;

/// The labels of the name at `offset`, following compression pointers.
///
/// A pointer names a prior occurrence of the name's suffix (RFC 1035
/// section 4.1.4), so each pointer must land before the one followed
/// last, or before the name itself for the first; a chain of them then
/// cannot loop, and with the octet limit a name decodes in a bounded
/// number of steps however the message is built.
struct Labels<'a> {
    bytes: &'a [u8],
    offset: usize,
    /// Every compression pointer must land below this offset.
    pointer_limit: usize,
    /// Wire octets decoded so far, length octets included.
    octets: usize,
    /// Where the name ends in the message, once the decoder has passed
    /// its root label or its first pointer.
    end_in_place: Option<usize>,
    finished: bool,
}

impl<'a> Labels<'a> {
    fn new(bytes: &'a [u8], offset: usize) -> Self {
        Self {
            bytes,
            offset,
            pointer_limit: offset,
            octets: 0,
            end_in_place: None,
            finished: false,
        }
    }

    fn fail(&mut self) -> Option<Result<&'a [u8], MalformedName>> {
        self.finished = true;
        Some(Err(MalformedName))
    }
}

impl<'a> Iterator for Labels<'a> {
    type Item = Result<&'a [u8], MalformedName>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        loop {
            let Some(&len) = self.bytes.get(self.offset) else {
                return self.fail();
            };
            match len & 0xc0 {
                0xc0 => {
                    let Some(pointer) = read_u16(self.bytes, self.offset) else {
                        return self.fail();
                    };
                    let target = usize::from(pointer & 0x3fff);
                    if target >= self.pointer_limit {
                        return self.fail();
                    }
                    self.end_in_place.get_or_insert(self.offset + 2);
                    self.pointer_limit = target;
                    self.offset = target;
                }
                0 if len == 0 => {
                    self.octets += 1;
                    if self.octets > MAX_NAME_OCTETS {
                        return self.fail();
                    }
                    self.end_in_place.get_or_insert(self.offset + 1);
                    self.finished = true;
                    return None;
                }
                0 => {
                    let start = self.offset + 1;
                    let end = start + usize::from(len);
                    self.octets += 1 + usize::from(len);
                    if self.octets > MAX_NAME_OCTETS {
                        return self.fail();
                    }
                    let Some(label) = self.bytes.get(start..end) else {
                        return self.fail();
                    };
                    self.offset = end;
                    return Some(Ok(label));
                }
                _ => return self.fail(),
            }
        }
    }
}

/// Whether the names at two offsets are the same name, labels compared
/// without regard to ASCII case. `None` when either does not decode.
fn names_equal(bytes: &[u8], left: usize, right: usize) -> Option<bool> {
    let mut left = Labels::new(bytes, left);
    let mut right = Labels::new(bytes, right);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Some(true),
            (Some(Ok(left)), Some(Ok(right))) => {
                if !left.eq_ignore_ascii_case(right) {
                    return Some(false);
                }
            }
            (Some(Err(MalformedName)), _) | (_, Some(Err(MalformedName))) => return None,
            (Some(Ok(_)), None) | (None, Some(Ok(_))) => return Some(false),
        }
    }
}

/// Whether the name at `offset` is `text` (dotted labels), ignoring
/// ASCII case. `None` when the name does not decode.
fn name_equals_text(bytes: &[u8], offset: usize, text: &str) -> Option<bool> {
    let mut wire = Labels::new(bytes, offset);
    let mut text = text.split('.');
    loop {
        match (wire.next(), text.next()) {
            (None, None) => return Some(true),
            (Some(Ok(wire)), Some(text)) => {
                if !wire.eq_ignore_ascii_case(text.as_bytes()) {
                    return Some(false);
                }
            }
            (Some(Err(MalformedName)), _) => return None,
            (Some(Ok(_)), None) | (None, Some(_)) => return Some(false),
        }
    }
}

/// Whether the name at `name` is one of the `owners`. `None` when a name
/// does not decode.
fn is_owner(bytes: &[u8], owners: &[usize], name: usize) -> Option<bool> {
    for &owner in owners {
        if names_equal(bytes, owner, name)? {
            return Some(true);
        }
    }
    Some(false)
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

    const EXAMPLE_A: DnsQuery<'static> = DnsQuery {
        name: "example.com",
        record: DnsRecordType::A,
    };
    const EXAMPLE_AAAA: DnsQuery<'static> = DnsQuery {
        name: "example.com",
        record: DnsRecordType::Aaaa,
    };

    /// Where an answer record's owner name comes from.
    enum Owner<'a> {
        /// A compression pointer to the question's name, the way real
        /// resolvers write it.
        Question,
        /// A name written out in full.
        Name(&'a str),
    }

    fn push_name(bytes: &mut alloc::vec::Vec<u8>, name: &str) {
        for label in name.split('.') {
            bytes.push(label.len() as u8);
            bytes.extend_from_slice(label.as_bytes());
        }
        bytes.push(0);
    }

    fn push_record(
        bytes: &mut alloc::vec::Vec<u8>,
        owner: &Owner<'_>,
        record_type: u16,
        data: &[u8],
    ) {
        match owner {
            Owner::Question => bytes.extend_from_slice(&[0xc0, 0x0c]),
            Owner::Name(name) => push_name(bytes, name),
        }
        bytes.extend_from_slice(&record_type.to_be_bytes());
        bytes.extend_from_slice(&CLASS_IN.to_be_bytes());
        bytes.extend_from_slice(&60u32.to_be_bytes());
        bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
        bytes.extend_from_slice(data);
    }

    /// A response whose question is `name` / `question_type` / `class`.
    fn response_for(
        name: &str,
        question_type: u16,
        class: u16,
        answers: &[(Owner<'_>, u16, alloc::vec::Vec<u8>)],
    ) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        bytes.extend_from_slice(&0x1234u16.to_be_bytes());
        bytes.extend_from_slice(&0x8180u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        push_name(&mut bytes, name);
        bytes.extend_from_slice(&question_type.to_be_bytes());
        bytes.extend_from_slice(&class.to_be_bytes());
        for (owner, record_type, data) in answers {
            push_record(&mut bytes, owner, *record_type, data);
        }
        bytes
    }

    fn wire_name(name: &str) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        push_name(&mut bytes, name);
        bytes
    }

    fn response_with(answers: &[(u16, &[u8])]) -> alloc::vec::Vec<u8> {
        let answers: alloc::vec::Vec<_> = answers
            .iter()
            .map(|(record_type, data)| (Owner::Question, *record_type, data.to_vec()))
            .collect();
        response_for("example.com", TYPE_A, CLASS_IN, &answers)
    }

    fn parse(bytes: &[u8], query: DnsQuery<'_>) -> Option<DnsMessage> {
        DnsResponse::parse(bytes).and_then(|response| response.message(query))
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
        assert_eq!(u16::from_be_bytes([a[a_len - 4], a[a_len - 3]]), TYPE_A);
        assert_eq!(
            u16::from_be_bytes([aaaa[aaaa_len - 4], aaaa[aaaa_len - 3]]),
            TYPE_AAAA
        );
        assert_eq!(
            u16::from_be_bytes([aaaa[aaaa_len - 2], aaaa[aaaa_len - 1]]),
            CLASS_IN
        );
    }

    const EXAMPLE_V6: [u8; 16] = [
        0x26, 0x06, 0x28, 0x00, 0x02, 0x1f, 0xcb, 0x07, 0x68, 0x20, 0x80, 0xda, 0x00, 0xaf, 0x6b,
        0x08,
    ];

    #[test]
    fn response_collects_the_queried_family() {
        let bytes = response_with(&[(TYPE_A, &[93, 184, 215, 14])]);
        let message = parse(&bytes, EXAMPLE_A).expect("response should parse");

        assert_eq!(message.id, 0x1234);
        assert_eq!(
            message.ipv4_addresses().collect::<alloc::vec::Vec<_>>(),
            alloc::vec![Ipv4Address::new([93, 184, 215, 14])]
        );

        let bytes = response_for(
            "example.com",
            TYPE_AAAA,
            CLASS_IN,
            &[(Owner::Question, TYPE_AAAA, EXAMPLE_V6.to_vec())],
        );
        let message = parse(&bytes, EXAMPLE_AAAA).expect("response should parse");
        assert_eq!(
            message.ipv6_addresses().collect::<alloc::vec::Vec<_>>(),
            alloc::vec![Ipv6Address::new(EXAMPLE_V6)]
        );
    }

    #[test]
    fn records_of_another_type_than_the_question_are_not_used() {
        let bytes = response_with(&[(TYPE_A, &[93, 184, 215, 14]), (TYPE_AAAA, &EXAMPLE_V6)]);
        let message = parse(&bytes, EXAMPLE_A).expect("response should parse");

        assert_eq!(message.addresses.len(), 1);
    }

    #[test]
    fn answer_for_another_name_is_rejected() {
        let bytes = response_for(
            "attacker.example",
            TYPE_A,
            CLASS_IN,
            &[(Owner::Question, TYPE_A, alloc::vec![6, 6, 6, 6])],
        );

        assert_eq!(parse(&bytes, EXAMPLE_A), None);
    }

    #[test]
    fn answer_for_another_type_or_class_is_rejected() {
        let wrong_type = response_for(
            "example.com",
            TYPE_AAAA,
            CLASS_IN,
            &[(Owner::Question, TYPE_AAAA, EXAMPLE_V6.to_vec())],
        );
        assert_eq!(parse(&wrong_type, EXAMPLE_A), None);

        let wrong_class = response_for(
            "example.com",
            TYPE_A,
            3,
            &[(Owner::Question, TYPE_A, alloc::vec![6, 6, 6, 6])],
        );
        assert_eq!(parse(&wrong_class, EXAMPLE_A), None);
    }

    #[test]
    fn response_without_exactly_one_question_is_rejected() {
        let mut bytes = response_with(&[(TYPE_A, &[1, 2, 3, 4])]);
        bytes[5] = 0;
        assert_eq!(parse(&bytes, EXAMPLE_A), None);
    }

    #[test]
    fn question_name_matches_without_regard_to_case() {
        // A resolver that applies 0x20 case randomisation echoes the
        // question in the case it was sent; RFC 1035 section 2.3.3 makes
        // the comparison case-insensitive either way.
        let bytes = response_for(
            "ExAmPlE.CoM",
            TYPE_A,
            CLASS_IN,
            &[(Owner::Question, TYPE_A, alloc::vec![1, 2, 3, 4])],
        );

        assert_eq!(
            parse(&bytes, EXAMPLE_A)
                .expect("case differences are not mismatches")
                .addresses
                .len(),
            1
        );
    }

    #[test]
    fn records_owned_by_unrelated_names_are_dropped() {
        let bytes = response_for(
            "example.com",
            TYPE_A,
            CLASS_IN,
            &[
                (
                    Owner::Name("attacker.example"),
                    TYPE_A,
                    alloc::vec![6, 6, 6, 6],
                ),
                (Owner::Question, TYPE_A, alloc::vec![1, 2, 3, 4]),
            ],
        );
        let message = parse(&bytes, EXAMPLE_A).expect("response should parse");

        assert_eq!(
            message.addresses.as_slice(),
            &[IpAddress::Ipv4(Ipv4Address::new([1, 2, 3, 4]))]
        );
    }

    #[test]
    fn records_owned_by_the_cname_chain_are_used_in_any_order() {
        // example.com -> www.example.net -> edge.example.org, listed with
        // the far link first and the address owned by the chain's end.
        let bytes = response_for(
            "example.com",
            TYPE_A,
            CLASS_IN,
            &[
                (
                    Owner::Name("EDGE.example.org"),
                    TYPE_A,
                    alloc::vec![1, 2, 3, 4],
                ),
                (
                    Owner::Name("www.example.net"),
                    TYPE_CNAME,
                    wire_name("edge.example.org"),
                ),
                (Owner::Question, TYPE_CNAME, wire_name("www.example.net")),
                (
                    Owner::Name("unrelated.example"),
                    TYPE_A,
                    alloc::vec![6, 6, 6, 6],
                ),
            ],
        );
        let message = parse(&bytes, EXAMPLE_A).expect("response should parse");

        assert_eq!(
            message.addresses.as_slice(),
            &[IpAddress::Ipv4(Ipv4Address::new([1, 2, 3, 4]))]
        );
    }

    #[test]
    fn a_cname_owned_by_an_unrelated_name_extends_nothing() {
        let bytes = response_for(
            "example.com",
            TYPE_A,
            CLASS_IN,
            &[
                (
                    Owner::Name("attacker.example"),
                    TYPE_CNAME,
                    wire_name("victim.example"),
                ),
                (
                    Owner::Name("victim.example"),
                    TYPE_A,
                    alloc::vec![6, 6, 6, 6],
                ),
            ],
        );
        let message = parse(&bytes, EXAMPLE_A).expect("response should parse");

        assert!(message.addresses.is_empty());
    }

    #[test]
    fn a_cname_chain_past_the_limit_is_not_followed() {
        // example.com -> l1.example -> ... -> l12.example: one link more
        // than the limit, so l11.example is the last name trusted.
        let links: alloc::vec::Vec<alloc::string::String> = (1..=DNS_MAX_CNAME_CHAIN + 1)
            .map(|link| alloc::format!("l{link}.example"))
            .collect();
        let mut answers = alloc::vec![(Owner::Question, TYPE_CNAME, wire_name(&links[0]))];
        for pair in links.windows(2) {
            answers.push((Owner::Name(&pair[0]), TYPE_CNAME, wire_name(&pair[1])));
        }
        answers.push((
            Owner::Name(&links[DNS_MAX_CNAME_CHAIN - 1]),
            TYPE_A,
            alloc::vec![1, 2, 3, 4],
        ));
        answers.push((
            Owner::Name(&links[DNS_MAX_CNAME_CHAIN]),
            TYPE_A,
            alloc::vec![6, 6, 6, 6],
        ));
        let bytes = response_for("example.com", TYPE_A, CLASS_IN, &answers);
        let message = parse(&bytes, EXAMPLE_A).expect("response should parse");

        assert_eq!(
            message.addresses.as_slice(),
            &[IpAddress::Ipv4(Ipv4Address::new([1, 2, 3, 4]))]
        );
    }

    #[test]
    fn a_cname_loop_ends_the_chain() {
        let bytes = response_for(
            "example.com",
            TYPE_A,
            CLASS_IN,
            &[
                (Owner::Question, TYPE_CNAME, wire_name("alias.example")),
                (
                    Owner::Name("alias.example"),
                    TYPE_CNAME,
                    wire_name("example.com"),
                ),
                (
                    Owner::Name("alias.example"),
                    TYPE_A,
                    alloc::vec![1, 2, 3, 4],
                ),
            ],
        );
        let message = parse(&bytes, EXAMPLE_A).expect("response should parse");

        assert_eq!(
            message.addresses.as_slice(),
            &[IpAddress::Ipv4(Ipv4Address::new([1, 2, 3, 4]))]
        );
    }

    #[test]
    fn a_compression_pointer_that_does_not_point_back_rejects_the_response() {
        let mut bytes = response_for("example.com", TYPE_A, CLASS_IN, &[]);
        bytes[7] = 1;
        // The answer's owner is the label `a` followed by a pointer back
        // to that label: a loop a decoder would follow forever.
        let owner = u16::try_from(bytes.len()).expect("test message fits a pointer");
        bytes.extend_from_slice(&[1, b'a']);
        bytes.extend_from_slice(&(0xc000 | owner).to_be_bytes());
        bytes.extend_from_slice(&TYPE_A.to_be_bytes());
        bytes.extend_from_slice(&CLASS_IN.to_be_bytes());
        bytes.extend_from_slice(&60u32.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&[6, 6, 6, 6]);

        assert_eq!(parse(&bytes, EXAMPLE_A), None);
    }

    #[test]
    fn a_name_longer_than_255_octets_rejects_the_response() {
        // Four 63-octet labels and the root are 257 octets on the wire.
        let label = "a".repeat(63);
        let name = [label.as_str(); 4].join(".");
        let bytes = response_for(
            &name,
            TYPE_A,
            CLASS_IN,
            &[(Owner::Question, TYPE_A, alloc::vec![6, 6, 6, 6])],
        );
        let query = DnsQuery {
            name: &name,
            record: DnsRecordType::A,
        };

        assert_eq!(parse(&bytes, query), None);
        assert_eq!(
            DnsQuestionWriter::new(&mut [0u8; 512]).write_a_query(1, &name),
            None
        );
    }

    #[test]
    fn malformed_address_records_are_skipped_without_failing_the_response() {
        // A CNAME and an AAAA record whose RDATA length lies about the
        // family must not stop the AAAA record behind them from parsing.
        let bytes = response_for(
            "example.com",
            TYPE_AAAA,
            CLASS_IN,
            &[
                (Owner::Question, TYPE_CNAME, alloc::vec![0xc0, 0x0c]),
                (Owner::Question, TYPE_AAAA, alloc::vec![1, 2, 3]),
                (
                    Owner::Question,
                    TYPE_AAAA,
                    alloc::vec![0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3],
                ),
            ],
        );
        let message = parse(&bytes, EXAMPLE_AAAA).expect("response should parse");

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
        let message = parse(&bytes, EXAMPLE_A).expect("oversized response should still parse");

        assert_eq!(message.addresses.len(), DNS_MAX_ADDRESS_RECORDS);
    }
}
