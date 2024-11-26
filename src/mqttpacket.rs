use std::io::{self, Write};
use std::str;

// variable byte integer
fn parse_int(src: &[u8]) -> Option<Result<(u32, usize), io::Error>> {
    let mut pos = 0;
    let mut value = 0;

    while pos == 0 || src[pos - 1] & 0x80 != 0 {
        if pos >= src.len() {
            return None;
        }

        if pos >= 4 {
            return Some(Err(io::ErrorKind::InvalidData.into()));
        }

        value += ((src[pos] & 0x7f) as u32) << (pos * 7);
        pos += 1;
    }

    Some(Ok((value, pos)))
}

// variable byte integer
fn write_int<W: Write>(dest: &mut W, value: u32) -> Result<(), io::Error> {
    let mut wrote = false;
    let mut remaining = value;

    while !wrote || remaining > 0 {
        let mut b = (remaining & 0x7f) as u8;

        remaining >>= 7;
        if remaining > 0 {
            b |= 0x80;
        }

        dest.write_all(&[b])?;
        wrote = true;
    }

    Ok(())
}

fn parse_binary(src: &[u8]) -> Result<(&[u8], usize), io::Error> {
    if src.len() < 2 {
        return Err(io::ErrorKind::InvalidData.into());
    }

    let len = u16::from_be_bytes(src[..2].try_into().unwrap()) as usize;

    if src.len() < 2 + len {
        return Err(io::ErrorKind::InvalidData.into());
    }

    let s = &src[2..(2 + len)];

    Ok((s, 2 + len))
}

fn parse_string(src: &[u8]) -> Result<(&str, usize), io::Error> {
    let (data, read) = parse_binary(src)?;

    let Ok(s) = str::from_utf8(data) else {
        return Err(io::ErrorKind::InvalidData.into());
    };

    Ok((s, read))
}

#[repr(u8)]
#[derive(Debug, Copy, Clone)]
pub enum Reason {
    Success = 0x00,
    NoSubscriptionExisted = 0x11,
    UnspecifiedError = 0x80,
    ProtocolError = 0x82,
    UnsupportedProtocolVersion = 0x84,
    NotAuthorized = 0x87,
    WildcardSubscriptionsNotSupported = 0xa2,
}

#[derive(Debug)]
pub struct Connect<'a> {
    pub version: u8,
    pub client_id: &'a str,
    pub password: Option<&'a str>,
}

#[derive(Debug)]
pub struct ConnAck {
    pub reason: Reason,
}

#[derive(Debug)]
pub struct ConnAckV4 {
    pub ret: u8,
}

#[derive(Debug)]
pub struct Disconnect;

#[derive(Debug)]
pub struct PingReq;

#[derive(Debug)]
pub struct PingResp;

#[derive(Debug)]
pub struct Subscribe<'a> {
    pub id: u16,
    pub topic: &'a str,
}

#[derive(Debug)]
pub struct SubAck {
    pub id: u16,
    pub reason: Reason,
}

#[derive(Debug)]
pub struct Unsubscribe<'a> {
    pub id: u16,
    pub topic: &'a str,
}

#[derive(Debug)]
pub struct UnsubAck {
    pub id: u16,
    pub reason: Reason,
}

#[derive(Debug)]
pub struct Publish<'a> {
    pub topic: &'a str,
    pub message: &'a [u8],
}

#[derive(Debug)]
pub enum Packet<'a> {
    Connect(Connect<'a>),
    ConnAck(ConnAck),
    ConnAckV4(ConnAckV4),
    Disconnect(Disconnect),
    PingReq(PingReq),
    PingResp(PingResp),
    Subscribe(Subscribe<'a>),
    SubAck(SubAck),
    Unsubscribe(Unsubscribe<'a>),
    UnsubAck(UnsubAck),
    Publish(Publish<'a>),
    Unsupported(u8),
}

impl<'a> Packet<'a> {
    pub fn parse(src: &'a [u8]) -> Option<Result<(Self, usize), io::Error>> {
        if src.len() < 2 {
            return None;
        }

        let ptype = src[0] >> 4;
        let flags = src[0] & 0x0f;

        let src = &src[1..];

        let (len, len_read) = match parse_int(src)? {
            Ok(ret) => ret,
            Err(e) => return Some(Err(e)),
        };

        let len = len as usize;
        let src = &src[len_read..];

        if src.len() < len {
            return None;
        }

        let packet_size = 1 + len_read + len;

        let p = match ptype {
            1 => {
                // protocol name
                let (name, read) = match parse_string(src) {
                    Ok(s) => s,
                    Err(e) => return Some(Err(e)),
                };

                let src = &src[read..];

                // name and version number
                if name != "MQTT" || src.is_empty() {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let version = src[0];

                if version != 5 {
                    // treat as limited packet with version number only
                    return Some(Ok((
                        Self::Connect(Connect {
                            version,
                            client_id: "",
                            password: None,
                        }),
                        packet_size,
                    )));
                }

                let src = &src[1..];

                // flags and keep-alive
                if src.len() < 3 {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let cflags = src[0];

                let src = &src[3..];

                let (props_len, read) = match parse_int(src) {
                    Some(Ok(ret)) => ret,
                    Some(Err(e)) => return Some(Err(e)),
                    None => return Some(Err(io::ErrorKind::InvalidData.into())),
                };

                let props_len = props_len as usize;
                let src = &src[read..];

                if src.len() < props_len {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let src = &src[props_len..];

                let (client_id, read) = match parse_string(src) {
                    Ok(s) => s,
                    Err(e) => return Some(Err(e)),
                };

                let mut src = &src[read..];

                // will
                if cflags & 0x04 != 0 {
                    let (will_props_len, read) = match parse_int(src) {
                        Some(Ok(ret)) => ret,
                        Some(Err(e)) => return Some(Err(e)),
                        None => return Some(Err(io::ErrorKind::InvalidData.into())),
                    };

                    let will_props_len = will_props_len as usize;
                    src = &src[read..];

                    if src.len() < will_props_len {
                        return Some(Err(io::ErrorKind::InvalidData.into()));
                    }

                    src = &src[will_props_len..];

                    // will topic
                    let (_, read) = match parse_string(src) {
                        Ok(s) => s,
                        Err(e) => return Some(Err(e)),
                    };

                    src = &src[read..];

                    // will payload
                    let (_, read) = match parse_binary(src) {
                        Ok(s) => s,
                        Err(e) => return Some(Err(e)),
                    };

                    src = &src[read..];
                }

                // username
                if cflags & 0x80 != 0 {
                    let (_, read) = match parse_string(src) {
                        Ok(s) => s,
                        Err(e) => return Some(Err(e)),
                    };

                    src = &src[read..];
                }

                let mut password = None;

                // password
                if cflags & 0x40 != 0 {
                    let (s, _) = match parse_string(src) {
                        Ok(s) => s,
                        Err(e) => return Some(Err(e)),
                    };

                    password = Some(s);
                }

                Self::Connect(Connect {
                    version,
                    client_id,
                    password,
                })
            }
            3 => {
                // QoS must be 0
                if flags & 0x06 > 0 {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let (topic, read) = match parse_string(src) {
                    Ok(s) => s,
                    Err(e) => return Some(Err(e)),
                };

                let src = &src[read..];

                let (props_len, read) = match parse_int(src) {
                    Some(Ok(ret)) => ret,
                    Some(Err(e)) => return Some(Err(e)),
                    None => return Some(Err(io::ErrorKind::InvalidData.into())),
                };

                let props_len = props_len as usize;
                let src = &src[read..];

                if src.len() < props_len {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let message = &src[props_len..];

                Self::Publish(Publish { topic, message })
            }
            8 => {
                // spec says flags must be set to 2, without explanation
                if flags != 0x02 {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                if src.len() < 2 {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let id = u16::from_be_bytes(src[..2].try_into().unwrap());

                let src = &src[2..];

                let (props_len, read) = match parse_int(src) {
                    Some(Ok(ret)) => ret,
                    Some(Err(e)) => return Some(Err(e)),
                    None => return Some(Err(io::ErrorKind::InvalidData.into())),
                };

                let props_len = props_len as usize;
                let src = &src[read..];

                if src.len() < props_len {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let src = &src[props_len..];

                let (topic, _) = match parse_string(src) {
                    Ok(s) => s,
                    Err(e) => return Some(Err(e)),
                };

                Self::Subscribe(Subscribe { id, topic })
            }
            10 => {
                if src.len() < 2 {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let id = u16::from_be_bytes(src[..2].try_into().unwrap());

                let src = &src[2..];

                let (props_len, read) = match parse_int(src) {
                    Some(Ok(ret)) => ret,
                    Some(Err(e)) => return Some(Err(e)),
                    None => return Some(Err(io::ErrorKind::InvalidData.into())),
                };

                let props_len = props_len as usize;
                let src = &src[read..];

                if src.len() < props_len {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let src = &src[props_len..];

                let (topic, _) = match parse_string(src) {
                    Ok(s) => s,
                    Err(e) => return Some(Err(e)),
                };

                Self::Unsubscribe(Unsubscribe { id, topic })
            }
            12 => Self::PingReq(PingReq),
            14 => Self::Disconnect(Disconnect),
            ptype => Self::Unsupported(ptype),
        };

        Some(Ok((p, packet_size)))
    }

    pub fn serialize<W: Write>(&self, dest: &mut W) -> Result<(), io::Error> {
        let mut out = Vec::new();

        match self {
            Self::ConnAck(ConnAck { reason }) => {
                out.push(0x20); // type=2 flags=0
                write_int(&mut out, 3)?; // remaining length

                out.push(0x00); // acknowledge flags
                out.push(*reason as u8);
                write_int(&mut out, 0)?; // property length
            }
            Self::ConnAckV4(ConnAckV4 { ret }) => {
                out.push(0x20); // type=2 flags=0
                write_int(&mut out, 2)?; // remaining length

                out.push(0x00); // acknowledge flags
                out.push(*ret);
            }
            Self::PingResp(_) => {
                out.push(0xd0); // type=13 flags=0
                write_int(&mut out, 0)?; // remaining length
            }
            Self::SubAck(SubAck { id, reason }) => {
                out.push(0x90); // type=9 flags=0
                write_int(&mut out, 4)?; // remaining length

                out.extend(&id.to_be_bytes());
                write_int(&mut out, 0)?; // property length
                out.push(*reason as u8);
            }
            Self::UnsubAck(UnsubAck { id, reason }) => {
                out.push(0x90); // type=11 flags=0
                write_int(&mut out, 4)?; // remaining length

                out.extend(&id.to_be_bytes());
                write_int(&mut out, 0)?; // property length
                out.push(*reason as u8);
            }
            Self::Publish(Publish { topic, message }) => {
                out.push(0x30); // type=3 flags=0

                let len = (topic.len() + 3 + message.len()) as u32;
                write_int(&mut out, len)?; // remaining length

                out.extend(&(topic.len() as u16).to_be_bytes());
                out.extend(topic.as_bytes());
                write_int(&mut out, 0)?; // property length

                out.extend(*message);
            }
            _ => panic!("cannot serialize type"),
        }

        dest.write_all(&out)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(data: &[u8]) -> String {
        let mut out = String::new();

        for (n, c) in hex::encode(data).chars().enumerate() {
            if n > 0 && n % 2 == 0 {
                out.push(' ');
            }

            out.push(c);
        }

        out
    }

    #[test]
    fn packet() {
        let topic = "fruit";
        let message = "apple".as_bytes();

        let p = Packet::Publish(Publish { topic, message });

        let mut data = Vec::new();
        p.serialize(&mut data).unwrap();

        let expected = "30 0d 00 05 66 72 75 69 74 00 61 70 70 6c 65";
        assert_eq!(hex(&data), expected);

        let (p, read) = Packet::parse(&data).unwrap().unwrap();
        assert_eq!(read, 15);

        let publish = match p {
            Packet::Publish(p) => p,
            _ => panic!("unexpected packet type"),
        };

        assert_eq!(publish.topic, "fruit");
        assert_eq!(publish.message, b"apple");
    }
}
