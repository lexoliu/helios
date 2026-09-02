//! Platform real-time clock contracts.
//!
//! A processor's timer counts from reset and says nothing about the
//! calendar. Every machine Helios targets carries a separate device
//! that does: a PL031 on the AArch64 virt board, a goldfish RTC on the
//! RISC-V one, the mc146818 CMOS bank on x86, the host OS on a hosted
//! backend. The kernel reads one of them once at boot to place its
//! monotonic clock on the wall.
//!
//! The devices disagree about the encoding, not about the meaning. Some
//! hand back a Unix counter, others a broken-down calendar in
//! binary-coded decimal. The encoding-independent parts — the epoch
//! value type, the calendar conversion and the BCD digit rule — live
//! here, so a backend driver only has to describe its register map.

use thiserror::Error;

/// Nanoseconds in one second, as the kernel's clocks count them.
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Seconds since the Unix epoch, as a real-time clock reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixSeconds(u64);

impl UnixSeconds {
    pub const fn new(seconds: u64) -> Self {
        Self(seconds)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The same instant counted in nanoseconds, which is the unit every
    /// clock inside the kernel works in.
    pub const fn as_nanos(self) -> u128 {
        (self.0 as u128) * (NANOS_PER_SECOND as u128)
    }
}

/// Why a real-time clock could not be read.
///
/// Every variant describes a device that answered but answered with
/// something no calendar can mean; a device that is simply absent is
/// reported by the backend's discovery instead, which returns no clock
/// at all.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum RtcError {
    #[error("real-time clock register {register} holds {value:#04x}, which is not a BCD pair")]
    NotBinaryCodedDecimal { register: &'static str, value: u8 },
    #[error("real-time clock reported {field}={value}, which no calendar allows")]
    FieldOutOfRange { field: &'static str, value: u32 },
    #[error("real-time clock reported year {year}, which precedes the Unix epoch")]
    BeforeEpoch { year: u16 },
    #[error("real-time clock kept changing across {attempts} consecutive reads")]
    Unsettled { attempts: u32 },
}

/// A device that keeps calendar time while the processor's timer only
/// counts.
///
/// The kernel reads it once, during bring-up, and never polls it again:
/// the monotonic timer carries time forward from there. Implementations
/// are therefore free to take as long as the register protocol needs.
pub trait RealTimeClock {
    /// Names the backing device in the kernel's boot log and in any
    /// report of where wall time came from.
    const SOURCE: &'static str;

    fn read(&self) -> Result<UnixSeconds, RtcError>;
}

/// A broken-down calendar reading, as register-per-field clocks report
/// it.
///
/// The fields are in the units a human calendar uses — a four-digit
/// year, months and days counted from one — so a driver that reads BCD
/// registers converts the digits, not the ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalendarTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl CalendarTime {
    /// Converts a UTC calendar reading to the Unix epoch counter.
    ///
    /// A leap second (`second == 60`) is carried through as the 61st
    /// second of its minute rather than rejected; every other field is
    /// range-checked against the calendar, including the length of the
    /// month in the given year.
    pub fn to_unix_seconds(self) -> Result<UnixSeconds, RtcError> {
        if self.year < UNIX_EPOCH_YEAR {
            return Err(RtcError::BeforeEpoch { year: self.year });
        }
        if self.month == 0 || self.month > 12 {
            return Err(RtcError::FieldOutOfRange {
                field: "month",
                value: u32::from(self.month),
            });
        }
        if self.day == 0 || self.day > days_in_month(self.year, self.month) {
            return Err(RtcError::FieldOutOfRange {
                field: "day",
                value: u32::from(self.day),
            });
        }
        if self.hour > 23 {
            return Err(RtcError::FieldOutOfRange {
                field: "hour",
                value: u32::from(self.hour),
            });
        }
        if self.minute > 59 {
            return Err(RtcError::FieldOutOfRange {
                field: "minute",
                value: u32::from(self.minute),
            });
        }
        if self.second > 60 {
            return Err(RtcError::FieldOutOfRange {
                field: "second",
                value: u32::from(self.second),
            });
        }

        let days = days_from_epoch(self.year, self.month, self.day);
        let seconds = days * SECONDS_PER_DAY
            + u64::from(self.hour) * 3_600
            + u64::from(self.minute) * 60
            + u64::from(self.second);
        Ok(UnixSeconds::new(seconds))
    }
}

/// How a register-per-field clock encodes what it reports.
///
/// Such clocks declare their own encoding in a status register, and a
/// board is free to ship either combination, so the driver reads the
/// declaration and hands it here rather than assuming one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalendarEncoding {
    /// Fields hold plain binary rather than binary-coded decimal.
    pub binary: bool,
    /// Hours run 0..=23 rather than 1..=12 with an afternoon flag in
    /// the register's top bit.
    pub twenty_four_hour: bool,
}

/// The register bytes a calendar clock reports, before the encoding its
/// status register declares has been undone.
///
/// The year register holds two digits; the century either comes from a
/// second register the platform points at or from the era the caller
/// knows the machine to be in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawCalendar {
    pub second: u8,
    pub minute: u8,
    pub hour: u8,
    pub day: u8,
    pub month: u8,
    pub year: u8,
    /// The century register, on a clock whose platform points at one.
    pub century: Option<u8>,
}

/// The top bit of the hour register on a clock reporting 12-hour time.
const AFTERNOON_FLAG: u8 = 0x80;

impl RawCalendar {
    /// Undoes the clock's declared encoding.
    ///
    /// `default_century` is the century a clock whose platform points at
    /// no century register is taken to be in — the era the machine
    /// ships in, not a guess the clock made.
    pub fn decode(
        self,
        encoding: CalendarEncoding,
        default_century: u8,
    ) -> Result<CalendarTime, RtcError> {
        let afternoon = !encoding.twenty_four_hour && self.hour & AFTERNOON_FLAG != 0;
        let hour = self.hour & !AFTERNOON_FLAG;

        let decode = |register, value| {
            if encoding.binary {
                Ok(value)
            } else {
                from_binary_coded_decimal(register, value)
            }
        };

        let second = decode("second", self.second)?;
        let minute = decode("minute", self.minute)?;
        let hour = decode("hour", hour)?;
        let day = decode("day", self.day)?;
        let month = decode("month", self.month)?;
        let year_of_century = decode("year", self.year)?;
        let century = match self.century {
            Some(century) => decode("century", century)?,
            None => default_century,
        };

        // 12-hour clocks number noon and midnight as 12, so the hour
        // wraps to zero before the afternoon flag adds its half day.
        let hour = if encoding.twenty_four_hour {
            hour
        } else {
            if hour == 0 || hour > 12 {
                return Err(RtcError::FieldOutOfRange {
                    field: "hour",
                    value: u32::from(hour),
                });
            }
            (hour % 12) + if afternoon { 12 } else { 0 }
        };

        Ok(CalendarTime {
            year: u16::from(century) * 100 + u16::from(year_of_century),
            month,
            day,
            hour,
            minute,
            second,
        })
    }
}

/// Decodes one binary-coded-decimal register byte.
///
/// `register` names the register in the error, so a clock that answers
/// with a half-updated field says which one it was.
pub fn from_binary_coded_decimal(register: &'static str, value: u8) -> Result<u8, RtcError> {
    let high = value >> 4;
    let low = value & 0x0f;
    if high > 9 || low > 9 {
        return Err(RtcError::NotBinaryCodedDecimal { register, value });
    }
    Ok(high * 10 + low)
}

const UNIX_EPOCH_YEAR: u16 = 1970;
const SECONDS_PER_DAY: u64 = 86_400;

const fn is_leap_year(year: u16) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days between 1970-01-01 and the given date, by Howard Hinnant's
/// civil-from-days algorithm shifted to the Unix epoch.
///
/// The caller has already range-checked the date, so the arithmetic
/// here cannot leave the calendar.
fn days_from_epoch(year: u16, month: u8, day: u8) -> u64 {
    // The algorithm counts years from March so that the leap day lands
    // at the end of a year and needs no special case.
    let shifted_year = u32::from(year) - u32::from(month <= 2);
    let era = shifted_year / 400;
    let year_of_era = shifted_year - era * 400;
    let shifted_month = u32::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * shifted_month + 2) / 5 + u32::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_from_civil_epoch = u64::from(era) * 146_097 + u64::from(day_of_era);
    // 719_468 days separate 0000-03-01 from 1970-01-01.
    days_from_civil_epoch - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calendar(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> CalendarTime {
        CalendarTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    #[test]
    fn epoch_and_known_instants_convert_exactly() {
        assert_eq!(
            calendar(1970, 1, 1, 0, 0, 0).to_unix_seconds(),
            Ok(UnixSeconds::new(0))
        );
        assert_eq!(
            calendar(2001, 9, 9, 1, 46, 40).to_unix_seconds(),
            Ok(UnixSeconds::new(1_000_000_000))
        );
        assert_eq!(
            calendar(2026, 9, 1, 12, 34, 56).to_unix_seconds(),
            Ok(UnixSeconds::new(1_788_266_096))
        );
        // The 32-bit PL031 counter wraps here, which is the instant the
        // AArch64 driver's register width stops being able to express.
        assert_eq!(
            calendar(2038, 1, 19, 3, 14, 7).to_unix_seconds(),
            Ok(UnixSeconds::new(2_147_483_647))
        );
    }

    #[test]
    fn leap_days_and_century_rules_shift_the_count() {
        // 2000 is a leap year (divisible by 400) and 2100 is not, which
        // is the rule a CMOS century register exists to disambiguate.
        assert_eq!(
            calendar(2000, 2, 29, 0, 0, 0).to_unix_seconds(),
            Ok(UnixSeconds::new(951_782_400))
        );
        assert_eq!(
            calendar(2000, 3, 1, 0, 0, 0).to_unix_seconds(),
            Ok(UnixSeconds::new(951_868_800))
        );
        assert!(calendar(2100, 2, 29, 0, 0, 0).to_unix_seconds().is_err());
        assert_eq!(
            calendar(2024, 2, 29, 23, 59, 59).to_unix_seconds(),
            Ok(UnixSeconds::new(1_709_251_199))
        );
    }

    #[test]
    fn one_day_of_arithmetic_stays_consistent() {
        let midnight = calendar(2026, 9, 1, 0, 0, 0)
            .to_unix_seconds()
            .expect("a valid date must convert");
        let end_of_day = calendar(2026, 9, 1, 23, 59, 59)
            .to_unix_seconds()
            .expect("a valid date must convert");
        let next_midnight = calendar(2026, 9, 2, 0, 0, 0)
            .to_unix_seconds()
            .expect("a valid date must convert");
        assert_eq!(end_of_day.get() - midnight.get(), 86_399);
        assert_eq!(next_midnight.get() - midnight.get(), 86_400);
    }

    #[test]
    fn a_leap_second_is_carried_rather_than_rejected() {
        let leap = calendar(2016, 12, 31, 23, 59, 60)
            .to_unix_seconds()
            .expect("a leap second must be carried through");
        let previous = calendar(2016, 12, 31, 23, 59, 59)
            .to_unix_seconds()
            .expect("a valid date must convert");
        assert_eq!(leap.get() - previous.get(), 1);
    }

    #[test]
    fn out_of_range_fields_name_themselves() {
        assert_eq!(
            calendar(1969, 12, 31, 23, 59, 59).to_unix_seconds(),
            Err(RtcError::BeforeEpoch { year: 1969 })
        );
        assert_eq!(
            calendar(2026, 13, 1, 0, 0, 0).to_unix_seconds(),
            Err(RtcError::FieldOutOfRange {
                field: "month",
                value: 13
            })
        );
        assert_eq!(
            calendar(2026, 4, 31, 0, 0, 0).to_unix_seconds(),
            Err(RtcError::FieldOutOfRange {
                field: "day",
                value: 31
            })
        );
        assert_eq!(
            calendar(2026, 1, 1, 24, 0, 0).to_unix_seconds(),
            Err(RtcError::FieldOutOfRange {
                field: "hour",
                value: 24
            })
        );
        assert_eq!(
            calendar(2026, 1, 1, 0, 60, 0).to_unix_seconds(),
            Err(RtcError::FieldOutOfRange {
                field: "minute",
                value: 60
            })
        );
        assert_eq!(
            calendar(2026, 1, 1, 0, 0, 61).to_unix_seconds(),
            Err(RtcError::FieldOutOfRange {
                field: "second",
                value: 61
            })
        );
    }

    #[test]
    fn binary_coded_decimal_digits_decode_and_invalid_ones_are_rejected() {
        assert_eq!(from_binary_coded_decimal("seconds", 0x00), Ok(0));
        assert_eq!(from_binary_coded_decimal("seconds", 0x09), Ok(9));
        assert_eq!(from_binary_coded_decimal("minutes", 0x59), Ok(59));
        assert_eq!(from_binary_coded_decimal("year", 0x99), Ok(99));
        assert_eq!(
            from_binary_coded_decimal("hours", 0x1a),
            Err(RtcError::NotBinaryCodedDecimal {
                register: "hours",
                value: 0x1a
            })
        );
        assert_eq!(
            from_binary_coded_decimal("day", 0xf0),
            Err(RtcError::NotBinaryCodedDecimal {
                register: "day",
                value: 0xf0
            })
        );
    }

    const BCD_24H: CalendarEncoding = CalendarEncoding {
        binary: false,
        twenty_four_hour: true,
    };
    const BINARY_24H: CalendarEncoding = CalendarEncoding {
        binary: true,
        twenty_four_hour: true,
    };
    const BCD_12H: CalendarEncoding = CalendarEncoding {
        binary: false,
        twenty_four_hour: false,
    };

    #[test]
    fn a_binary_coded_decimal_register_set_decodes_to_its_calendar() {
        let raw = RawCalendar {
            second: 0x56,
            minute: 0x34,
            hour: 0x12,
            day: 0x01,
            month: 0x09,
            year: 0x26,
            century: Some(0x20),
        };
        assert_eq!(
            raw.decode(BCD_24H, 20),
            Ok(calendar(2026, 9, 1, 12, 34, 56))
        );
    }

    #[test]
    fn a_binary_register_set_decodes_without_digit_rules() {
        let raw = RawCalendar {
            second: 56,
            minute: 34,
            hour: 23,
            day: 31,
            month: 12,
            year: 26,
            century: Some(20),
        };
        assert_eq!(
            raw.decode(BINARY_24H, 20),
            Ok(calendar(2026, 12, 31, 23, 34, 56))
        );
    }

    #[test]
    fn a_clock_without_a_century_register_uses_the_platforms_era() {
        let raw = RawCalendar {
            second: 0x00,
            minute: 0x00,
            hour: 0x00,
            day: 0x01,
            month: 0x01,
            year: 0x99,
            century: None,
        };
        assert_eq!(raw.decode(BCD_24H, 20), Ok(calendar(2099, 1, 1, 0, 0, 0)));
        assert_eq!(raw.decode(BCD_24H, 19), Ok(calendar(1999, 1, 1, 0, 0, 0)));
    }

    #[test]
    fn twelve_hour_registers_fold_noon_and_midnight_correctly() {
        let at_hour = |hour: u8| RawCalendar {
            second: 0x00,
            minute: 0x00,
            hour,
            day: 0x01,
            month: 0x01,
            year: 0x26,
            century: Some(0x20),
        };
        // 12 AM is hour zero, 12 PM is noon, and an afternoon hour adds
        // half a day.
        assert_eq!(
            at_hour(0x12).decode(BCD_12H, 20),
            Ok(calendar(2026, 1, 1, 0, 0, 0))
        );
        assert_eq!(
            at_hour(0x12 | AFTERNOON_FLAG).decode(BCD_12H, 20),
            Ok(calendar(2026, 1, 1, 12, 0, 0))
        );
        assert_eq!(
            at_hour(0x01).decode(BCD_12H, 20),
            Ok(calendar(2026, 1, 1, 1, 0, 0))
        );
        assert_eq!(
            at_hour(0x11 | AFTERNOON_FLAG).decode(BCD_12H, 20),
            Ok(calendar(2026, 1, 1, 23, 0, 0))
        );
        // A 24-hour clock keeps the same byte as hour 18 rather than
        // reading the top bit as an afternoon flag.
        assert_eq!(
            at_hour(0x18).decode(BCD_24H, 20),
            Ok(calendar(2026, 1, 1, 18, 0, 0))
        );
    }

    #[test]
    fn a_twelve_hour_clock_rejects_an_hour_outside_its_numbering() {
        let raw = RawCalendar {
            second: 0x00,
            minute: 0x00,
            hour: 0x00,
            day: 0x01,
            month: 0x01,
            year: 0x26,
            century: Some(0x20),
        };
        assert_eq!(
            raw.decode(BCD_12H, 20),
            Err(RtcError::FieldOutOfRange {
                field: "hour",
                value: 0
            })
        );
    }

    #[test]
    fn a_half_updated_register_is_reported_against_its_own_name() {
        let raw = RawCalendar {
            second: 0x56,
            minute: 0xff,
            hour: 0x12,
            day: 0x01,
            month: 0x09,
            year: 0x26,
            century: Some(0x20),
        };
        assert_eq!(
            raw.decode(BCD_24H, 20),
            Err(RtcError::NotBinaryCodedDecimal {
                register: "minute",
                value: 0xff
            })
        );
    }

    #[test]
    fn unix_seconds_convert_to_the_kernels_nanosecond_unit() {
        assert_eq!(UnixSeconds::new(0).as_nanos(), 0);
        assert_eq!(
            UnixSeconds::new(1_788_266_096).as_nanos(),
            1_788_266_096_000_000_000
        );
    }
}
