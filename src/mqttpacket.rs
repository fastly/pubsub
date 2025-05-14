use std::borrow::Cow;
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
    QoSNotSupported = 0x9b,
    WildcardSubscriptionsNotSupported = 0xa2,
}

impl TryFrom<u8> for Reason {
    type Error = ();

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            x if x == Self::Success as u8 => Ok(Self::Success),
            x if x == Self::NoSubscriptionExisted as u8 => Ok(Self::NoSubscriptionExisted),
            x if x == Self::UnspecifiedError as u8 => Ok(Self::UnspecifiedError),
            x if x == Self::ProtocolError as u8 => Ok(Self::ProtocolError),
            x if x == Self::UnsupportedProtocolVersion as u8 => {
                Ok(Self::UnsupportedProtocolVersion)
            }
            x if x == Self::NotAuthorized as u8 => Ok(Self::NotAuthorized),
            x if x == Self::QoSNotSupported as u8 => Ok(Self::QoSNotSupported),
            x if x == Self::WildcardSubscriptionsNotSupported as u8 => {
                Ok(Self::WildcardSubscriptionsNotSupported)
            }
            _ => Err(()),
        }
    }
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
    pub maximum_packet_size: Option<u32>,
}

#[derive(Debug)]
pub struct ConnAckV4 {
    pub ret: u8,
}

#[derive(Debug)]
pub struct Disconnect {
    pub reason: Reason,
}

#[derive(Debug)]
pub struct PingReq;

#[derive(Debug)]
pub struct PingResp;

#[derive(Debug)]
pub struct Subscribe<'a> {
    pub id: u16,
    pub topic: &'a str,
    pub maximum_qos: u8,
    pub no_local: bool,
    pub retain_as_published: bool,
    pub retain_handling: u8,
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
    pub topic: Cow<'a, str>,
    pub message: Cow<'a, [u8]>,
    pub dup: bool,
    pub qos: u8,
    pub retain: bool,
    pub message_expiry_interval: Option<u32>,
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
                let retain = flags & 0x01 > 0;
                let qos = (flags >> 1) & 0x03;
                let dup = flags & 0x08 > 0;

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

                let mut message_expiry_interval = None;

                let mut psrc = &src[..props_len];
                while !psrc.is_empty() {
                    match psrc[0] {
                        0x01 => {
                            // payload format

                            if psrc.len() < 2 {
                                return Some(Err(io::ErrorKind::InvalidData.into()));
                            }

                            psrc = &psrc[2..];
                        }
                        0x02 => {
                            // message expiry interval

                            if psrc.len() < 5 {
                                return Some(Err(io::ErrorKind::InvalidData.into()));
                            }

                            message_expiry_interval =
                                Some(u32::from_be_bytes(psrc[1..5].try_into().unwrap()));

                            psrc = &psrc[5..];
                        }
                        0x23 => {
                            // topic alias

                            if psrc.len() < 3 {
                                return Some(Err(io::ErrorKind::InvalidData.into()));
                            }

                            psrc = &psrc[3..];
                        }
                        0x08 => {
                            // response topic

                            let (_, read) = match parse_string(&psrc[1..]) {
                                Ok(s) => s,
                                Err(e) => return Some(Err(e)),
                            };

                            psrc = &psrc[(1 + read)..];
                        }
                        0x09 => {
                            // correlation data

                            let (_, read) = match parse_binary(&psrc[1..]) {
                                Ok(s) => s,
                                Err(e) => return Some(Err(e)),
                            };

                            psrc = &psrc[(1 + read)..];
                        }
                        0x26 => {
                            // user property

                            let (_, read) = match parse_string(&psrc[1..]) {
                                Ok(s) => s,
                                Err(e) => return Some(Err(e)),
                            };

                            psrc = &psrc[(1 + read)..];

                            let (_, read) = match parse_string(psrc) {
                                Ok(s) => s,
                                Err(e) => return Some(Err(e)),
                            };

                            psrc = &psrc[read..];
                        }
                        0x0b => {
                            // subscription identifier

                            let (_, read) = match parse_int(&psrc[1..]) {
                                Some(Ok(ret)) => ret,
                                Some(Err(e)) => return Some(Err(e)),
                                None => return Some(Err(io::ErrorKind::InvalidData.into())),
                            };

                            psrc = &psrc[(1 + read)..];
                        }
                        0x03 => {
                            // content type

                            let (_, read) = match parse_string(&psrc[1..]) {
                                Ok(s) => s,
                                Err(e) => return Some(Err(e)),
                            };

                            psrc = &psrc[(1 + read)..];
                        }
                        _ => return Some(Err(io::ErrorKind::InvalidData.into())),
                    }
                }

                let message = &src[props_len..];

                Self::Publish(Publish {
                    topic: Cow::from(topic),
                    message: Cow::from(message),
                    dup,
                    qos,
                    retain,
                    message_expiry_interval,
                })
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

                let (topic, read) = match parse_string(src) {
                    Ok(s) => s,
                    Err(e) => return Some(Err(e)),
                };

                let src = &src[read..];

                if src.is_empty() {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let opts = src[0];

                let maximum_qos = opts & 0x03;
                let no_local = opts & 0x04 != 0;
                let retain_as_published = opts & 0x08 != 0;
                let retain_handling = (opts >> 4) & 0x03;

                Self::Subscribe(Subscribe {
                    id,
                    topic,
                    maximum_qos,
                    no_local,
                    retain_as_published,
                    retain_handling,
                })
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
            14 => {
                let (vheader_len, read) = match parse_int(src) {
                    Some(Ok(ret)) => ret,
                    Some(Err(e)) => return Some(Err(e)),
                    None => return Some(Err(io::ErrorKind::InvalidData.into())),
                };

                let vheader_len = vheader_len as usize;
                let src = &src[read..];

                if src.len() < vheader_len {
                    return Some(Err(io::ErrorKind::InvalidData.into()));
                }

                let mut reason = 0;

                if !src.is_empty() {
                    reason = src[0];
                }

                Self::Disconnect(Disconnect {
                    reason: Reason::try_from(reason).unwrap_or(Reason::UnspecifiedError),
                })
            }
            ptype => Self::Unsupported(ptype),
        };

        Some(Ok((p, packet_size)))
    }

    pub fn serialize<W: Write>(&self, dest: &mut W) -> Result<(), io::Error> {
        let mut out = Vec::new();

        match self {
            Self::ConnAck(p) => {
                let mut props = vec![
                    0x24, // maximum qos
                    0x00, // QoS 0
                    0x25, // retain available
                    0x01, // yes
                ];

                if let Some(x) = p.maximum_packet_size {
                    // maximum packet size
                    props.push(0x27);
                    props.extend(x.to_be_bytes());
                }

                // wildcard subscription available
                props.push(0x28);
                props.push(0x00); // no

                // shared subscription available
                props.push(0x2a);
                props.push(0x00); // no

                let mut props_with_len = Vec::new();
                write_int(&mut props_with_len, props.len() as u32)?; // property length
                props_with_len.extend(&props);

                out.push(0x20); // type=2 flags=0
                write_int(&mut out, (props_with_len.len() + 2) as u32)?; // remaining length

                out.push(0x00); // acknowledge flags
                out.push(p.reason as u8);
                out.extend(&props_with_len);
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
            Self::Publish(p) => {
                let mut props = Vec::new();

                if let Some(x) = p.message_expiry_interval {
                    // message expiry interval
                    props.push(0x02);
                    props.extend(x.to_be_bytes());
                }

                let mut props_with_len = Vec::new();
                write_int(&mut props_with_len, props.len() as u32)?; // property length
                props_with_len.extend(&props);

                let mut flags = 0;

                if p.retain {
                    flags |= 0x01;
                }

                flags |= (p.qos & 0x03) << 1;

                if p.dup {
                    flags |= 0x08;
                }

                out.push(0x30 | flags); // type=3

                let len = (2 + p.topic.len() + props_with_len.len() + p.message.len()) as u32;
                write_int(&mut out, len)?; // remaining length

                out.extend(&(p.topic.len() as u16).to_be_bytes());
                out.extend(p.topic.as_bytes());

                out.extend(&props_with_len);

                out.extend(p.message.as_ref());
            }
            Self::Disconnect(Disconnect { reason }) => {
                out.push(0xe0); // type 14

                write_int(&mut out, 1)?;
                out.push(*reason as u8);
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

        let p = Packet::Publish(Publish {
            topic: Cow::from(topic),
            message: Cow::from(message),
            dup: false,
            qos: 0,
            retain: false,
            message_expiry_interval: None,
        });

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
        assert_eq!(publish.message.as_ref(), b"apple");
        assert!(!publish.dup);
        assert_eq!(publish.qos, 0);
        assert!(!publish.retain);
        assert!(publish.message_expiry_interval.is_none());

        let p = Packet::Publish(Publish {
            topic: Cow::from(topic),
            message: Cow::from(message),
            dup: true,
            qos: 1,
            retain: true,
            message_expiry_interval: Some(30),
        });

        let mut data = Vec::new();
        p.serialize(&mut data).unwrap();

        let expected = "3b 12 00 05 66 72 75 69 74 05 02 00 00 00 1e 61 70 70 6c 65";
        assert_eq!(hex(&data), expected);

        let (p, read) = Packet::parse(&data).unwrap().unwrap();
        assert_eq!(read, 20);

        let publish = match p {
            Packet::Publish(p) => p,
            _ => panic!("unexpected packet type"),
        };

        assert_eq!(publish.topic, "fruit");
        assert_eq!(publish.message.as_ref(), b"apple");
        assert!(publish.dup);
        assert_eq!(publish.qos, 1);
        assert!(publish.retain);
        assert_eq!(publish.message_expiry_interval, Some(30));
    }
}
