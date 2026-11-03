//! Shared low-level XML scanning primitives.

pub fn unescape_xml_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut decoded = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(ampersand) = remaining.find('&') {
        decoded.push_str(&remaining[..ampersand]);
        remaining = &remaining[ampersand + 1..];
        let Some(semicolon) = remaining.find(';') else {
            decoded.push('&');
            decoded.push_str(remaining);
            remaining = "";
            break;
        };
        let entity = &remaining[..semicolon];
        let character = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            value if value.starts_with("#x") || value.starts_with("#X") => {
                u32::from_str_radix(&value[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            value if value.starts_with('#') => {
                value[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(ch) = character {
            decoded.push(ch);
        } else {
            decoded.push('&');
            decoded.push_str(entity);
            decoded.push(';');
        }
        remaining = &remaining[semicolon + 1..];
    }
    decoded.push_str(remaining);
    decoded
}
