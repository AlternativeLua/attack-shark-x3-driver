//! Wire format of the Attack Shark X3 configuration protocol. Pure: no I/O.
//!
//! The mouse is configured through HID feature reports on interface 2, each
//! of which rewrites one whole block of settings:
//!
//! | id   | block                                                              |
//! |------|--------------------------------------------------------------------|
//! | 0x04 | sensor: DPI stages, active stage, lift-off, ripple, angle snap, motion sync |
//! | 0x05 | power: sleep, deep sleep, debounce                                 |
//! | 0x06 | polling rate                                                       |
//!
//! 0x04 and 0x05 are framed `id, len, 0x01, body.., sum_hi, sum_lo`, where the
//! sum is the 16-bit sum of the body bytes. 0x06 carries its one value
//! followed by its complement instead.
//!
//! The mouse reports back on the same interface with 5-byte input reports
//! `0x03, source, event, a, b`; see [`Event`].
//!
//! Byte layouts are those sent by the official Windows software, taken from
//! the USB captures published in DavBfr/shark-x3 (reproduced in the tests
//! below). The X3 shares its USB IDs with the Attack Shark R1 but not its
//! report 0x04 layout, so this must not be pointed at an R1.

use crate::settings::{DPI_MAX, DPI_STEP, STAGES, Settings, group};

pub const VENDOR_ID: u16 = 0x1d57;
pub const PRODUCT_WIRED: u16 = 0xfa61;
pub const PRODUCT_DONGLE: u16 = 0xfa60;

pub const REPORT_SENSOR: u8 = 0x04;
pub const REPORT_POWER: u8 = 0x05;
pub const REPORT_POLLING: u8 = 0x06;
/// Arms a single feature read of another report; see [`query_request`].
pub const REPORT_QUERY: u8 = 0xa0;
/// The id of the input reports decoded by [`Event::parse`].
pub const REPORT_EVENT: u8 = 0x03;

pub const SENSOR_LEN: usize = 52;
pub const POWER_LEN: usize = 13;
pub const POLLING_LEN: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Wired,
    Dongle,
}

impl Link {
    pub fn from_product(pid: u16) -> Option<Link> {
        match pid {
            PRODUCT_WIRED => Some(Link::Wired),
            PRODUCT_DONGLE => Some(Link::Dongle),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Link::Wired => "wired",
            Link::Dongle => "dongle",
        }
    }
}

// Bytes 5 (0x3f: all six stages enabled) and 25..49 (per-stage indicator
// colours, then 0x01) are sent as the official software sends them.
#[rustfmt::skip]
const SENSOR_TEMPLATE: [u8; SENSOR_LEN] = [
    REPORT_SENSOR, 0x38, 0x01, // header
    0x00, // 3: lift-off distance, 0 = 1 mm, 1 = 2 mm
    0x00, // 4: ripple control
    0x3f, // 5
    0x00, // 6: angle snap
    0x00, // 7: motion sync
    0, 0, 0, 0, 0, 0, 0, 0, // 8..16: DPI low bytes, one per stage (last two unused)
    0, 0, 0, 0, 0, 0, 0, 0, // 16..24: DPI high bytes
    0x01, // 24: active stage, 1-based
    0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x00, // 25..37
    0x00, 0xff, 0xff, 0xff, 0x00, 0xff, 0xff, 0x40, 0x00, 0xff, 0xff, 0xff, // 37..49
    0x01, // 49
    0, 0, // 50..52: checksum
];

/// 16-bit sum of everything between the 3-byte header and the checksum.
fn body_sum(frame: &[u8]) -> u16 {
    frame[3..frame.len() - 2]
        .iter()
        .map(|&b| u16::from(b))
        .sum()
}

fn seal(frame: &mut [u8]) {
    let [hi, lo] = body_sum(frame).to_be_bytes();
    let n = frame.len();
    frame[n - 2] = hi;
    frame[n - 1] = lo;
}

pub fn sensor_report(s: &Settings) -> [u8; SENSOR_LEN] {
    let mut r = SENSOR_TEMPLATE;
    r[3] = u8::from(s.lod == 2);
    r[4] = u8::from(s.ripple);
    r[6] = u8::from(s.angle_snap);
    r[7] = u8::from(s.motion_sync);
    for (i, &dpi) in s.dpi.iter().enumerate() {
        let [hi, lo] = ((dpi / DPI_STEP - 1) as u16).to_be_bytes();
        r[8 + i] = lo;
        r[16 + i] = hi;
    }
    r[24] = s.stage;
    seal(&mut r);
    r
}

pub fn power_report(s: &Settings) -> [u8; POWER_LEN] {
    // Deep sleep is a 12-bit value: low byte in [5], top nibble in the high
    // nibble of [4]. The low nibble of [4] and bytes [3], [6..9] are LED
    // settings on sibling models; kept as the official software sends them.
    let deep = u16::from(s.deep_sleep) * 16 + 8;
    let [deep_hi, deep_lo] = deep.to_be_bytes();
    let mut r = [
        REPORT_POWER,
        0x0f,
        0x01,
        0x00,
        deep_hi << 4 | 0x03,
        deep_lo,
        0x00,
        0x00,
        0xff,
        s.sleep_half_min,
        s.debounce / 2,
        0,
        0,
    ];
    seal(&mut r);
    r
}

pub fn polling_report(hz: u16) -> [u8; POLLING_LEN] {
    let code = (1000 / hz) as u8;
    [REPORT_POLLING, 0x09, 0x01, code, !code, 0, 0, 0, 0]
}

/// The reports covering `groups`, in the order they should be sent.
pub fn frames(s: &Settings, groups: u8) -> Vec<(&'static str, Vec<u8>)> {
    let mut out = Vec::new();
    if groups & group::SENSOR != 0 {
        out.push(("sensor settings", sensor_report(s).to_vec()));
    }
    if groups & group::POLLING != 0 {
        out.push(("polling rate", polling_report(s.polling).to_vec()));
    }
    if groups & group::POWER != 0 {
        out.push(("power settings", power_report(s).to_vec()));
    }
    out
}

/// What actually goes on the wire for `frame` over `link`.
///
/// Over the cable the reports are sent at the size the HID descriptor
/// declares. Through the dongle, 0x04 and 0x05 are zero-padded to the length
/// in their header byte (56 and 15), as sibling models' drivers do.
pub fn padded(frame: &[u8], link: Link) -> Vec<u8> {
    let mut wire = frame.to_vec();
    if link == Link::Dongle && matches!(frame[0], REPORT_SENSOR | REPORT_POWER) {
        wire.resize(usize::from(frame[1]).max(frame.len()), 0);
    }
    wire
}

/// Feature report that arms one read of `report`; `len` is the read length
/// the firmware expects for it (the report's header length byte).
pub fn query_request(report: u8, len: u8) -> [u8; 8] {
    [REPORT_QUERY, report, len, 0x00, 0x01, 0x00, 0x00, 0x00]
}

fn check_frame(raw: &[u8], id: u8, len_byte: u8, min: usize) -> Result<(), String> {
    if raw.len() < min {
        return Err(format!("short report: {} of {min} bytes", raw.len()));
    }
    if raw[0] != id || raw[1] != len_byte {
        return Err(format!(
            "unexpected header {:02x} {:02x}, wanted {id:02x} {len_byte:02x}",
            raw[0], raw[1]
        ));
    }
    Ok(())
}

fn check_sum(frame: &[u8]) -> Result<(), String> {
    let stored = u16::from_be_bytes([frame[frame.len() - 2], frame[frame.len() - 1]]);
    let sum = body_sum(frame);
    if stored != sum {
        return Err(format!("checksum is {stored:04x}, body sums to {sum:04x}"));
    }
    Ok(())
}

fn flag(byte: u8, what: &str) -> Result<bool, String> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(format!("{what} byte is {other:#04x}, expected 0 or 1")),
    }
}

/// Reads a report 0x04 back into `s`. Strict on purpose: a value outside
/// what the X3 is known to use means the layout is not what we think it is.
pub fn decode_sensor(raw: &[u8], s: &mut Settings) -> Result<(), String> {
    check_frame(raw, REPORT_SENSOR, 0x38, SENSOR_LEN)?;
    let raw = &raw[..SENSOR_LEN];
    check_sum(raw)?;

    let mut dpi = [0; STAGES];
    for (i, slot) in dpi.iter_mut().enumerate() {
        *slot = (u32::from(u16::from_be_bytes([raw[16 + i], raw[8 + i]])) + 1) * DPI_STEP;
        if *slot > DPI_MAX {
            return Err(format!("stage {} decodes to {} DPI", i + 1, *slot));
        }
    }
    let stage = raw[24];
    if !(1..=STAGES as u8).contains(&stage) {
        return Err(format!("active stage byte is {stage:#04x}"));
    }
    let lod = flag(raw[3], "lift-off")?;
    let ripple = flag(raw[4], "ripple")?;
    let angle_snap = flag(raw[6], "angle snap")?;
    let motion_sync = flag(raw[7], "motion sync")?;

    s.dpi = dpi;
    s.stage = stage;
    s.lod = if lod { 2 } else { 1 };
    s.ripple = ripple;
    s.angle_snap = angle_snap;
    s.motion_sync = motion_sync;
    Ok(())
}

pub fn decode_power(raw: &[u8], s: &mut Settings) -> Result<(), String> {
    check_frame(raw, REPORT_POWER, 0x0f, POWER_LEN)?;
    let raw = &raw[..POWER_LEN];
    check_sum(raw)?;

    let deep = u16::from(raw[4] >> 4) << 8 | u16::from(raw[5]);
    if deep % 16 != 8 || !(1..=60).contains(&(deep / 16)) {
        return Err(format!("deep sleep field is {deep:#05x}"));
    }
    if !(1..=60).contains(&raw[9]) {
        return Err(format!("sleep byte is {:#04x}", raw[9]));
    }
    if !(2..=25).contains(&raw[10]) {
        return Err(format!("debounce byte is {:#04x}", raw[10]));
    }

    s.deep_sleep = (deep / 16) as u8;
    s.sleep_half_min = raw[9];
    s.debounce = raw[10] * 2;
    Ok(())
}

pub fn decode_polling(raw: &[u8], s: &mut Settings) -> Result<(), String> {
    check_frame(raw, REPORT_POLLING, 0x09, POLLING_LEN)?;
    if raw[4] != !raw[3] {
        return Err(format!(
            "code {:02x} and complement {:02x} disagree",
            raw[3], raw[4]
        ));
    }
    s.polling = match raw[3] {
        1 => 1000,
        2 => 500,
        4 => 250,
        8 => 125,
        code => return Err(format!("unknown polling code {code:#04x}")),
    };
    Ok(())
}

/// A decoded `0x03` input report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The DPI button was pressed; carries the new 1-based stage.
    DpiStage(u8),
    /// Periodic broadcast while the mouse is on the 2.4G link.
    Battery {
        state: u8,
        level: u8,
    },
    /// Sent once the mouse has processed feature report `report`; a
    /// `status` of 0 means it was accepted.
    Ack {
        status: u8,
        report: u8,
    },
    Other {
        code: u8,
        a: u8,
        b: u8,
    },
}

impl Event {
    pub fn parse(raw: &[u8]) -> Option<Event> {
        let &[REPORT_EVENT, _source, code, a, b, ..] = raw else {
            return None;
        };
        Some(match code {
            0x10 => Event::DpiStage(a),
            0x40 | 0x41 => Event::Battery { state: a, level: b },
            0x50 => Event::Ack {
                status: a,
                report: b,
            },
            _ => Event::Other { code, a, b },
        })
    }
}

/// The X3 reports its battery in tenths (0x0a = 100%); sibling models send
/// a percentage directly, so anything above 10 is taken as one.
pub fn battery_percent(level: u8) -> u8 {
    if level <= 10 {
        level * 10
    } else {
        level.min(100)
    }
}

pub fn battery_state(state: u8) -> Option<&'static str> {
    match state {
        1 => Some("on battery"),
        2 => Some("fully charged"),
        3 => Some("charging"),
        _ => None,
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        let mut s = Settings::default();
        for (key, value) in pairs {
            s.set(key, value).unwrap();
        }
        s
    }

    // Every vector below is a SET_REPORT payload captured from the official
    // Windows software (DavBfr/shark-x3, analysis/analysis.md), paired with
    // the settings that were on screen when it was sent.

    const SENSOR_CAPTURES: &[(&[(&str, &str)], &str)] = &[
        (
            &[("dpis", "750 1250 2700 5000 10000 20000"), ("stage", "4")],
            "04380100003f00000e183563c78f0000000000000001000004ff000000ff000000ffffff0000ffffff00ffff4000ffffff010f8c",
        ),
        (
            &[("dpis", "750 1250 2700 5000 10000 20000"), ("stage", "1")],
            "04380100003f00000e183563c78f0000000000000001000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010f89",
        ),
        (
            &[("dpis", "750 50 2700 5000 10000 20000"), ("stage", "1")],
            "04380100003f00000e003563c78f0000000000000001000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010f71",
        ),
        (
            &[
                ("dpis", "750 50 2700 5000 10000 20000"),
                ("stage", "1"),
                ("lod", "2"),
            ],
            "04380101003f00000e003563c78f0000000000000001000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010f72",
        ),
        (
            &[
                ("dpis", "750 50 2700 5000 10000 20000"),
                ("stage", "1"),
                ("lod", "2"),
                ("ripple", "on"),
            ],
            "04380101013f00000e003563c78f0000000000000001000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010f73",
        ),
        (
            &[
                ("dpis", "750 50 2700 5000 10000 20000"),
                ("stage", "1"),
                ("lod", "2"),
                ("ripple", "on"),
                ("angle-snap", "on"),
            ],
            "04380101013f01000e003563c78f0000000000000001000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010f74",
        ),
        (
            &[
                ("dpis", "750 50 2700 5000 10000 20000"),
                ("stage", "1"),
                ("lod", "2"),
                ("ripple", "on"),
                ("angle-snap", "on"),
                ("motion-sync", "on"),
            ],
            "04380101013f01010e003563c78f0000000000000001000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010f75",
        ),
        (
            &[
                ("dpis", "750 50 2700 5000 10000 400"),
                ("stage", "1"),
                ("lod", "2"),
                ("ripple", "on"),
                ("angle-snap", "on"),
                ("motion-sync", "on"),
            ],
            "04380101013f01010e003563c7070000000000000000000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010eec",
        ),
        (
            &[("dpis", "11600 50 2700 5000 10000 400"), ("stage", "1")],
            "04380100003f0000e7003563c7070000000000000000000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff010fc1",
        ),
        (
            &[("dpis", "11600 18100 2700 5000 10000 400"), ("stage", "1")],
            "04380100003f0000e7693563c7070000000100000000000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff01102b",
        ),
        (
            &[("dpis", "11600 18100 12250 5000 10000 400"), ("stage", "1")],
            "04380100003f0000e769f463c7070000000100000000000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff0110ea",
        ),
        (
            &[
                ("dpis", "11600 18100 12250 26000 10000 400"),
                ("stage", "1"),
            ],
            "04380100003f0000e769f407c7070000000100020000000001ff000000ff000000ffffff0000ffffff00ffff4000ffffff011090",
        ),
    ];

    const POWER_CAPTURES: &[(&[(&str, &str)], &str)] = &[
        (
            &[("sleep", "4.5"), ("deep-sleep", "10")],
            "050f010003a80000ff090301b6",
        ),
        (
            &[("sleep", "4.5"), ("deep-sleep", "25")],
            "050f010013980000ff090301b6",
        ),
        (
            &[("sleep", "2"), ("deep-sleep", "25")],
            "050f010013980000ff040301b1",
        ),
        (
            &[("sleep", "2"), ("deep-sleep", "60")],
            "050f010033c80000ff04030201",
        ),
        (
            &[("sleep", "30"), ("deep-sleep", "60")],
            "050f010033c80000ff3c030239",
        ),
        (
            &[("sleep", "27.5"), ("deep-sleep", "60"), ("debounce", "24")],
            "050f010033c80000ff370c023d",
        ),
        (
            &[("sleep", "27.5"), ("deep-sleep", "60"), ("debounce", "50")],
            "050f010033c80000ff3719024a",
        ),
        (
            &[("sleep", "2"), ("deep-sleep", "5")],
            "050f010003580000ff04030161",
        ),
        (
            &[("sleep", "2"), ("deep-sleep", "13")],
            "050f010003d80000ff040301e1",
        ),
        (
            &[("sleep", "2"), ("deep-sleep", "27")],
            "050f010013b80000ff040301d1",
        ),
        (
            &[("sleep", "8.5"), ("deep-sleep", "27")],
            "050f010013b80000ff110301de",
        ),
    ];

    const POLLING_CAPTURES: &[(u16, &str)] = &[
        (125, "06090108f700000000"),
        (250, "06090104fb00000000"),
        (500, "06090102fd00000000"),
        (1000, "06090101fe00000000"),
    ];

    #[test]
    fn sensor_report_matches_captures() {
        for (pairs, capture) in SENSOR_CAPTURES {
            let s = settings(pairs);
            assert_eq!(hex(&sensor_report(&s)), hex(&unhex(capture)), "{pairs:?}");
        }
    }

    #[test]
    fn power_report_matches_captures() {
        for (pairs, capture) in POWER_CAPTURES {
            let s = settings(pairs);
            assert_eq!(hex(&power_report(&s)), hex(&unhex(capture)), "{pairs:?}");
        }
    }

    #[test]
    fn polling_report_matches_captures() {
        for &(hz, capture) in POLLING_CAPTURES {
            assert_eq!(hex(&polling_report(hz)), hex(&unhex(capture)), "{hz} Hz");
        }
    }

    #[test]
    fn captures_decode_back_to_their_settings() {
        for (pairs, capture) in SENSOR_CAPTURES {
            let mut decoded = Settings::default();
            decode_sensor(&unhex(capture), &mut decoded).unwrap();
            assert_eq!(decoded, settings(pairs), "{pairs:?}");
        }
        for (pairs, capture) in POWER_CAPTURES {
            let mut decoded = Settings::default();
            decode_power(&unhex(capture), &mut decoded).unwrap();
            assert_eq!(decoded, settings(pairs), "{pairs:?}");
        }
        for &(hz, capture) in POLLING_CAPTURES {
            let mut decoded = Settings::default();
            decode_polling(&unhex(capture), &mut decoded).unwrap();
            assert_eq!(decoded.polling, hz);
        }
    }

    #[test]
    fn decode_accepts_dongle_padding_and_rejects_corruption() {
        let s = settings(&[("dpis", "400 800 1600 3200 6400 26000"), ("stage", "3")]);
        let wire = padded(&sensor_report(&s), Link::Dongle);
        assert_eq!(wire.len(), 56);
        let mut decoded = Settings::default();
        decode_sensor(&wire, &mut decoded).unwrap();
        assert_eq!(decoded, s);

        let mut corrupt = wire.clone();
        corrupt[9] ^= 0x01;
        assert!(
            decode_sensor(&corrupt, &mut decoded)
                .unwrap_err()
                .contains("checksum")
        );
        assert!(decode_sensor(&wire[..40], &mut decoded).is_err());
        assert!(decode_power(&wire, &mut decoded).is_err());
    }

    #[test]
    fn every_dpi_and_deep_sleep_value_round_trips() {
        let mut s = Settings::default();
        for dpi in (DPI_STEP..=DPI_MAX).step_by(DPI_STEP as usize) {
            s.dpi = [dpi; STAGES];
            let mut decoded = Settings::default();
            decode_sensor(&sensor_report(&s), &mut decoded).unwrap();
            assert_eq!(decoded.dpi, s.dpi);
        }
        for minutes in 1..=60 {
            s.deep_sleep = minutes;
            let mut decoded = Settings::default();
            decode_power(&power_report(&s), &mut decoded).unwrap();
            assert_eq!(decoded.deep_sleep, minutes);
        }
    }

    #[test]
    fn padding_depends_on_link_and_report() {
        let s = Settings::default();
        assert_eq!(padded(&sensor_report(&s), Link::Wired).len(), 52);
        assert_eq!(padded(&power_report(&s), Link::Wired).len(), 13);
        assert_eq!(padded(&power_report(&s), Link::Dongle).len(), 15);
        assert_eq!(padded(&polling_report(1000), Link::Dongle).len(), 9);
    }

    #[test]
    fn frames_cover_the_requested_groups() {
        let s = Settings::default();
        let ids = |groups| -> Vec<u8> { frames(&s, groups).iter().map(|(_, f)| f[0]).collect() };
        assert_eq!(ids(group::POLLING), [REPORT_POLLING]);
        assert_eq!(
            ids(group::SENSOR | group::POWER),
            [REPORT_SENSOR, REPORT_POWER]
        );
        assert_eq!(
            ids(group::ALL),
            [REPORT_SENSOR, REPORT_POLLING, REPORT_POWER]
        );
    }

    #[test]
    fn events() {
        assert_eq!(
            Event::parse(&[0x03, 0x00, 0x10, 0x04, 0x00]),
            Some(Event::DpiStage(4))
        );
        assert_eq!(
            Event::parse(&[0x03, 0x10, 0x40, 0x01, 0x07]),
            Some(Event::Battery { state: 1, level: 7 })
        );
        assert_eq!(
            Event::parse(&[0x03, 0x10, 0x50, 0x00, 0x04]),
            Some(Event::Ack {
                status: 0,
                report: 4
            })
        );
        assert_eq!(
            Event::parse(&[0x03, 0x10, 0x80, 0x01, 0x00]),
            Some(Event::Other {
                code: 0x80,
                a: 1,
                b: 0
            })
        );
        // Consumer-control and truncated reports are not events.
        assert_eq!(Event::parse(&[0x02, 0x23, 0x02]), None);
        assert_eq!(Event::parse(&[0x03, 0x10, 0x40]), None);

        assert_eq!(battery_percent(7), 70);
        assert_eq!(battery_percent(10), 100);
        assert_eq!(battery_percent(69), 69);
    }
}
