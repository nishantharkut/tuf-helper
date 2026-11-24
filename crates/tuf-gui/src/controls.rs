pub fn parse_charge_limit(text: &str) -> Option<u8> {
    let text = text.trim();
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u8>()
        .ok()
        .filter(|limit| (20..=100).contains(limit))
}

pub fn parse_hex(text: &str) -> Option<(u8, u8, u8)> {
    let text = text.trim();
    let text = text.strip_prefix('#').unwrap_or(text);
    if text.len() != 6 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |start| u8::from_str_radix(&text[start..start + 2], 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

pub fn aura_name(id: u32) -> String {
    match id {
        0 => "Static",
        1 => "Breathing",
        2 => "Rainbow cycle",
        3 => "Rainbow wave",
        4 => "Stars",
        5 => "Rain",
        6 => "Highlight",
        7 => "Laser",
        8 => "Ripple",
        10 => "Pulse",
        11 => "Comet",
        12 => "Flash",
        _ => return format!("Effect {id}"),
    }
    .into()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_complete_hex_colors() {
        assert_eq!(parse_hex(" #aB01FF "), Some((171, 1, 255)));
        assert_eq!(parse_hex("07dac1"), Some((7, 218, 193)));
        for invalid in ["", "###aabbcc", "123", "雪☀", "aabbccdd", "+12233"] {
            assert_eq!(parse_hex(invalid), None);
        }
    }

    #[test]
    fn charge_limit_rejects_incomplete_and_out_of_range_values() {
        assert_eq!(parse_charge_limit(" 77 "), Some(77));
        for invalid in ["", "19", "101", "77.9", "-77", "+77", "999", "７７"] {
            assert_eq!(parse_charge_limit(invalid), None);
        }
    }

    #[test]
    fn aura_options_follow_codes_reported_by_asusd() {
        assert_eq!(aura_name(2), "Rainbow cycle");
        assert_eq!(aura_name(3), "Rainbow wave");
        assert_eq!(aura_name(10), "Pulse");
        assert_eq!(aura_name(99), "Effect 99");
    }
}
