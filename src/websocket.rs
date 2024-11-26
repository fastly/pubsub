use std::str;

#[derive(Clone)]
pub struct WsEvent {
    pub etype: String,
    pub content: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct ControlMessage {
    #[serde(rename(serialize = "type"))]
    pub ctype: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    for (index, b) in haystack.iter().enumerate() {
        if *b == needle {
            return Some(index);
        }
    }

    None
}

pub struct ParseEventError;

pub fn parse_websocket_event(src: &[u8]) -> Result<(WsEvent, usize), ParseEventError> {
    let pos = match find_byte(src, b'\r') {
        Some(pos) => pos,
        None => return Err(ParseEventError),
    };

    let header = match str::from_utf8(&src[..pos]) {
        Ok(s) => s,
        Err(_) => return Err(ParseEventError),
    };

    let parts: Vec<&str> = header.split(" ").collect();

    let (content, size_so_far) = if parts.len() == 1 {
        // no content
        (&b""[..], header.len())
    } else if parts.len() == 2 {
        let clen = match usize::from_str_radix(parts[1], 16) {
            Ok(x) => x,
            Err(_) => return Err(ParseEventError),
        };

        if pos + 1 >= src.len() || src[pos + 1] != b'\n' {
            return Err(ParseEventError);
        }

        let remainder = &src[(pos + 2)..];

        if clen > remainder.len() {
            return Err(ParseEventError);
        }

        let content = &remainder[..clen];

        (content, header.len() + 2 + content.len())
    } else {
        return Err(ParseEventError);
    };

    let remainder = &src[size_so_far..];

    if remainder.len() < 2 {
        return Err(ParseEventError);
    }

    let footer = &remainder[..2];

    if footer != b"\r\n" {
        return Err(ParseEventError);
    }

    Ok((
        WsEvent {
            etype: parts[0].to_string(),
            content: content.to_vec(),
        },
        size_so_far + 2,
    ))
}
