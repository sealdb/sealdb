use std::cmp::Ordering;
use std::fmt::{Display, Formatter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Time {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub micro: u32,
}

impl Time {
    pub fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micro: 0,
        }
    }

    pub fn to_packed_u64(self) -> u64 {
        let ymd = (u64::from(self.year) * 13 + u64::from(self.month)) << 5 | u64::from(self.day);
        let hms =
            (u64::from(self.hour) << 12) | (u64::from(self.minute) << 6) | u64::from(self.second);
        ((ymd << 17) | hms) << 24 | u64::from(self.micro)
    }

    pub fn from_packed_u64(v: u64) -> Self {
        let ymdhms = v >> 24;
        let ymd = ymdhms >> 17;
        let day = (ymd & ((1 << 5) - 1)) as u8;
        let ym = ymd >> 5;
        let month = (ym % 13) as u8;
        let year = (ym / 13) as u16;
        let hms = ymdhms & ((1 << 17) - 1);
        let hour = (hms >> 12) as u8;
        let minute = ((hms >> 6) & 0x3f) as u8;
        let second = (hms & 0x3f) as u8;
        let micro = (v & ((1 << 24) - 1)) as u32;
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            micro,
        }
    }
}

impl Display for Time {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

impl PartialOrd for Time {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Time {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_packed_u64().cmp(&other.to_packed_u64())
    }
}
