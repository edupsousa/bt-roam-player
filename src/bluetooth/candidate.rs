//! Which devices count as audio speakers worth connecting to. Pure function over what
//! BlueZ tells us about a device, plus the `allow`/`deny` lists. See DESIGN.md, decision 1.

use bluer::{Address, Uuid};

/// A2DP Sink service class (`0000110b-0000-1000-8000-00805f9b34fb`).
pub const A2DP_SINK: Uuid = Uuid::from_u128(0x0000110b_0000_1000_8000_00805f9b34fb);

const MAJOR_AUDIO_VIDEO: u32 = 0x04;
/// Minor classes (of major Audio/Video) that output audio: wearable headset (earbuds such
/// as the WF-1000XM5 report this), loudspeaker, headphones, portable audio, car audio,
/// hi-fi.
const OUTPUT_MINORS: [u32; 6] = [0x01, 0x05, 0x06, 0x07, 0x08, 0x0a];
/// Video monitor / display and loudspeaker / conferencing / gaming: TVs and the like.
const VIDEO_MINORS: std::ops::RangeInclusive<u32> = 0x0c..=0x0f;

/// What BlueZ knows about a device. Any of it may be missing for a device seen only once.
#[derive(Debug, Clone, Copy)]
pub struct DeviceInfo<'a> {
    pub address: Address,
    pub name: Option<&'a str>,
    pub class: Option<u32>,
    pub uuids: &'a [Uuid],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Looks like an audio sink.
    Accept,
    /// Matches `deny`.
    Denied,
    /// `allow` is not empty and the device is not in it.
    NotAllowed,
    /// TV or other video device, even if it advertises A2DP.
    VideoDevice,
    /// Neither an A2DP sink nor an audio output class.
    NotAudio,
}

impl Verdict {
    pub fn is_accepted(self) -> bool {
        self == Self::Accept
    }
}

pub fn evaluate(info: &DeviceInfo, allow: &[String], deny: &[String]) -> Verdict {
    if deny.iter().any(|p| matches(p, info)) {
        return Verdict::Denied;
    }
    let allowed = allow.iter().any(|p| matches(p, info));
    if !allow.is_empty() && !allowed {
        return Verdict::NotAllowed;
    }
    let a2dp = info.uuids.contains(&A2DP_SINK);
    let (major, minor) = match info.class {
        Some(c) => (Some((c >> 8) & 0x1f), (c >> 2) & 0x3f),
        None => (None, 0),
    };
    let audio_major = major == Some(MAJOR_AUDIO_VIDEO);
    if audio_major && VIDEO_MINORS.contains(&minor) && !allowed {
        return Verdict::VideoDevice;
    }
    if a2dp || (audio_major && (OUTPUT_MINORS.contains(&minor) || allowed)) {
        Verdict::Accept
    } else {
        Verdict::NotAudio
    }
}

/// `pattern` is a Bluetooth address (case-insensitive) or a name pattern with `*` and `?`
/// wildcards (case-insensitive).
pub fn matches(pattern: &str, info: &DeviceInfo) -> bool {
    if let Ok(addr) = pattern.parse::<Address>() {
        return addr == info.address;
    }
    info.name.is_some_and(|name| glob(pattern, name))
}

fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    // Iterative wildcard match with backtracking to the last `*`.
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: Address = Address::new([0x04, 0xCB, 0x88, 0xAC, 0x79, 0x3E]);

    fn info(class: Option<u32>, uuids: &[Uuid]) -> DeviceInfo<'_> {
        DeviceInfo {
            address: ADDR,
            name: Some("JBL GO 2"),
            class,
            uuids,
        }
    }

    fn verdict(class: Option<u32>, uuids: &[Uuid]) -> Verdict {
        evaluate(&info(class, uuids), &[], &[])
    }

    #[test]
    fn accepts_the_real_speakers_from_m1() {
        // JBL GO 2, Echo Studio, WF-1000XM5.
        for class in [0x200414, 0x2c0414, 0x240404] {
            assert_eq!(verdict(Some(class), &[]), Verdict::Accept, "{class:#x}");
        }
    }

    #[test]
    fn rejects_the_tvs_from_m1_even_with_a2dp() {
        for class in [0x0c043c, 0x08043c] {
            assert_eq!(verdict(Some(class), &[]), Verdict::VideoDevice);
            assert_eq!(verdict(Some(class), &[A2DP_SINK]), Verdict::VideoDevice);
        }
    }

    #[test]
    fn rejects_other_device_classes() {
        // Phone, keyboard, mouse (0x002540 peripheral), watch.
        for class in [0x5a020c, 0x002540, 0x200704] {
            assert_eq!(verdict(Some(class), &[]), Verdict::NotAudio, "{class:#x}");
        }
        assert_eq!(verdict(None, &[]), Verdict::NotAudio);
    }

    #[test]
    fn a2dp_alone_is_enough() {
        assert_eq!(verdict(None, &[A2DP_SINK]), Verdict::Accept);
        assert_eq!(verdict(Some(0x5a020c), &[A2DP_SINK]), Verdict::Accept);
    }

    #[test]
    fn audio_class_with_a_non_output_minor_needs_a2dp() {
        // Hands-free (0x02), e.g. a car kit.
        assert_eq!(verdict(Some(0x200408), &[]), Verdict::NotAudio);
        assert_eq!(verdict(Some(0x200408), &[A2DP_SINK]), Verdict::Accept);
    }

    #[test]
    fn deny_wins_over_everything() {
        let i = info(Some(0x200414), &[A2DP_SINK]);
        let deny = vec!["04:cb:88:ac:79:3e".to_string()];
        let allow = vec!["JBL*".to_string()];
        assert_eq!(evaluate(&i, &allow, &deny), Verdict::Denied);
        assert_eq!(evaluate(&i, &[], &["jbl *".into()]), Verdict::Denied);
    }

    #[test]
    fn allow_list_restricts() {
        let i = info(Some(0x200414), &[]);
        assert_eq!(evaluate(&i, &["Bose*".into()], &[]), Verdict::NotAllowed);
        assert_eq!(evaluate(&i, &["JBL*".into()], &[]), Verdict::Accept);
        assert_eq!(evaluate(&i, &[ADDR.to_string()], &[]), Verdict::Accept);
    }

    #[test]
    fn allow_list_overrides_the_video_rejection_but_not_non_audio() {
        let tv = DeviceInfo {
            name: Some("Living room TV"),
            ..info(Some(0x0c043c), &[A2DP_SINK])
        };
        assert_eq!(evaluate(&tv, &["Living*".into()], &[]), Verdict::Accept);
        let phone = info(Some(0x5a020c), &[]);
        assert_eq!(evaluate(&phone, &["JBL*".into()], &[]), Verdict::NotAudio);
    }

    #[test]
    fn glob_matching() {
        assert!(glob("jbl*", "JBL GO 2"));
        assert!(glob("*GO*", "JBL GO 2"));
        assert!(glob("JBL ?? 2", "jbl go 2"));
        assert!(glob("*", ""));
        assert!(!glob("JBL", "JBL GO 2"));
        assert!(!glob("*Flip*", "JBL GO 2"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
    }

    #[test]
    fn nameless_device_matches_only_by_address() {
        let i = DeviceInfo {
            name: None,
            ..info(Some(0x200414), &[])
        };
        assert!(!matches("*", &i));
        assert!(matches("04:CB:88:AC:79:3E", &i));
    }
}
