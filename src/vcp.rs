//! VCP value mapping and monitor capability parsing.

/// Human-readable name for a VCP 0x60 "Input Select" value (MCCS 2.0 / 3.0).
pub fn input_name(code: u16) -> String {
    let n = match code {
        0x01 => "Analog 1",
        0x02 => "Analog 2",
        0x03 => "DVI 1",
        0x04 => "DVI 2",
        0x05 => "Composite 1",
        0x06 => "Composite 2",
        0x07 => "S-Video 1",
        0x08 => "S-Video 2",
        0x09 => "Tuner 1",
        0x0A => "Tuner 2",
        0x0B => "Tuner 3",
        0x0C => "Component 1",
        0x0D => "Component 2",
        0x0E => "Component 3",
        0x0F => "DisplayPort 1",
        0x10 => "DisplayPort 2",
        0x11 => "HDMI 1",
        0x12 => "HDMI 2",
        0x19 => "USB-C 1",
        0x1A => "USB-C 1 (alt)",
        0x1B => "USB-C 2",
        _ => return format!("Input {:#04x}", code),
    };
    n.to_string()
}

fn is_hex_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= 2 && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// Extract the values a monitor advertises for a given VCP code from its
/// capabilities string, e.g. `...vcp(02 10 60(11 12 0F 10))...` -> [0x11,0x12,0x0F,0x10].
///
/// Handles both `60(...)` and monitors that emit `60( ...)`.
pub fn parse_vcp_values(caps: &str, vcp: u8) -> Vec<u16> {
    let needle = format!("{:02X}", vcp);
    let bytes = caps.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // find a token boundary, then match the 2-hex-digit code
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        let token = &caps[start..i];
        if !token.eq_ignore_ascii_case(&needle) {
            continue;
        }
        // must be immediately followed by '(' for a value list
        if i >= bytes.len() || bytes[i] != b'(' {
            continue;
        }
        // collect the balanced parenthesised group
        let mut depth = 0usize;
        let mut j = i;
        while j < bytes.len() {
            match bytes[j] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let inner = &caps[i + 1..j.min(caps.len())];
        return inner
            .split_whitespace()
            .filter(|t| is_hex_token(t))
            .filter_map(|t| u16::from_str_radix(t, 16).ok())
            .collect();
    }
    Vec::new()
}

/// Fallback list when a monitor refuses to hand over its capabilities string.
/// These are the values monitors most commonly accept for input switching.
pub fn fallback_inputs() -> Vec<u16> {
    vec![0x0F, 0x10, 0x11, 0x12, 0x19, 0x1B]
}

/// Build the candidate input list: what the monitor advertises, or the
/// fallback set, always with `current` included so it stays selectable.
pub fn candidate_inputs(advertised: &[u16], current: Option<u16>) -> Vec<u16> {
    let mut v: Vec<u16> = if advertised.is_empty() {
        fallback_inputs()
    } else {
        advertised.to_vec()
    };
    if let Some(c) = current {
        if !v.contains(&c) {
            v.push(c);
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_caps_string() {
        // the exact string reported by the R32U81 in this session
        let caps = "(prot(monitor)type(lcd)MStarcmds(01 02 03 07 0C E3 F3)\
vcp(02 10 12 14(05 06 08 0B) 16 18 1A 52 60( 11 12 0F 10) 62 AA(01 02) AC AE B2 B6 C6 C8 C9 D6(01 04 05)  DF FD)mccs_ver(2.1)mswhql(1))";
        let mut got = parse_vcp_values(caps, 0x60);
        got.sort_unstable();
        assert_eq!(got, vec![0x0F, 0x10, 0x11, 0x12]);
    }

    #[test]
    fn parses_plain_list() {
        let caps = "vcp(10 12 60(0F 11 12) 62)";
        assert_eq!(parse_vcp_values(caps, 0x60), vec![0x0F, 0x11, 0x12]);
    }

    #[test]
    fn missing_vcp_returns_empty() {
        assert!(parse_vcp_values("vcp(10 12 62)", 0x60).is_empty());
    }

    #[test]
    fn nested_parens_do_not_leak() {
        let caps = "vcp(14(05 06) 60(0F 10))";
        assert_eq!(parse_vcp_values(caps, 0x60), vec![0x0F, 0x10]);
    }

    #[test]
    fn candidates_include_current() {
        let c = candidate_inputs(&[0x11, 0x12], Some(0x10));
        assert_eq!(c, vec![0x10, 0x11, 0x12]);
    }

    #[test]
    fn names_cover_common_ports() {
        assert_eq!(input_name(0x11), "HDMI 1");
        assert_eq!(input_name(0x10), "DisplayPort 2");
        assert_eq!(input_name(0xFE), "Input 0xfe");
    }
}

/// Resolve a user-supplied input specifier to a VCP 0x60 value.
///
/// Accepts a hex code (`0x11`, `11h`), a decimal code (`17`), or a friendly
/// name with separators ignored (`hdmi1`, `HDMI-1`, `dp2`, `displayport2`,
/// `usbc1`).
pub fn resolve_input(spec: &str) -> Option<u16> {
    let t = spec.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();

    // Explicit hex
    if let Some(rest) = lower.strip_prefix("0x") {
        return u16::from_str_radix(rest, 16).ok();
    }
    // Friendly names first, so "17" style decimals can't shadow "hdmi1"
    let norm: String = lower.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    for code in 0x01u16..=0x40 {
        let name: String = input_name(code)
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if name == norm {
            return Some(code);
        }
    }
    // Aliases monitors/KVMs are commonly labeled with
    for (alias, code) in [
        ("dp", 0x0Fu16),
        ("dp1", 0x0F),
        ("dp2", 0x10),
        ("displayport", 0x0F),
        ("displayport1", 0x0F),
        ("displayport2", 0x10),
        ("hdmi", 0x11),
        ("hdmi1", 0x11),
        ("hdmi2", 0x12),
        ("usbc", 0x19),
        ("usbc1", 0x19),
        ("usbc2", 0x1B),
        ("typec", 0x19),
        ("dvi", 0x03),
        ("vga", 0x01),
    ] {
        if alias == norm {
            return Some(code);
        }
    }
    // Bare decimal
    lower.parse::<u16>().ok()
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn resolves_hex_decimal_and_names() {
        assert_eq!(resolve_input("0x11"), Some(0x11));
        assert_eq!(resolve_input("17"), Some(17));
        assert_eq!(resolve_input("HDMI-1"), Some(0x11));
        assert_eq!(resolve_input("hdmi2"), Some(0x12));
        assert_eq!(resolve_input("dp2"), Some(0x10));
        assert_eq!(resolve_input("DisplayPort 1"), Some(0x0F));
        assert_eq!(resolve_input("usb-c 1"), Some(0x19));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(resolve_input("nonsense"), None);
        assert_eq!(resolve_input(""), None);
    }
}
