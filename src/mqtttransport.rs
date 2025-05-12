use crate::auth::Authorizor;
use crate::config::Config;
use crate::mqtthandler;
use crate::mqttpacket::Packet;
use crate::websocket::{parse_websocket_event, ControlMessage, WsEvent};
use fastly::http::{HeaderValue, StatusCode};
use fastly::{Body, Request, Response};
use std::collections::HashSet;
use std::io::Write;
use std::mem;
use std::str;

struct Context<'a> {
    handler_ctx: mqtthandler::Context<'a>,
    cid: String,
    in_buf: Vec<u8>,
    content_accepted: usize,
}

fn handle_websocket_event<H>(ctx: &mut Context, e: WsEvent, mut handler: H) -> Vec<WsEvent>
where
    H: for<'a> FnMut(&mut mqtthandler::Context, Packet<'a>) -> Vec<Packet<'a>>,
{
    let mut out_events = Vec::new();
    let mut content_accepted = e.content.len();

    println!("{} event {} size={}", ctx.cid, e.etype, e.content.len());

    match e.etype.as_str() {
        "OPEN" => out_events.push(e.clone()),  // ack
        "CLOSE" => out_events.push(e.clone()), // ack
        "TEXT" | "BINARY" => {
            content_accepted = 0;

            let mut in_buf = mem::take(&mut ctx.in_buf);

            in_buf.extend(e.content);

            while let Some(ret) = Packet::parse(&in_buf) {
                let (p, read) = match ret {
                    Ok(ret) => ret,
                    Err(_) => {
                        ctx.handler_ctx.disconnect = true;
                        break;
                    }
                };

                println!("{} IN {:?}", ctx.cid, p);

                for p in handler(&mut ctx.handler_ctx, p) {
                    println!("{} OUT {:?}", ctx.cid, p);

                    let mut buf = Vec::new();

                    // websocket-over-http messages must be prefixed
                    write!(&mut buf, "m:").unwrap();

                    p.serialize(&mut buf).unwrap();

                    out_events.push(WsEvent {
                        etype: "BINARY".to_string(),
                        content: buf,
                    });
                }

                in_buf = in_buf.split_off(read);
                content_accepted += read;
            }

            ctx.in_buf = in_buf;
        }
        _ => {} // unsupported event type, ignore
    }

    ctx.content_accepted += content_accepted;

    out_events
}

fn bad_request<T: AsRef<str>>(message: T) -> Response {
    Response::from_status(400).with_body_text_plain(&format!("{}\n", message.as_ref()))
}

fn handle_websocket_events<H>(
    config: &Config,
    authorizor: &dyn Authorizor,
    req: Request,
    body: Vec<u8>,
    mut handler: H,
) -> Response
where
    H: for<'a> FnMut(&mut mqtthandler::Context, Packet<'a>) -> Vec<Packet<'a>>,
{
    let mut grip_offered = false;
    let mut cid = String::new();
    let mut state = mqtthandler::State::default();
    let mut connected_subs = HashSet::new();

    if let Some(v) = req.get_header("Sec-WebSocket-Extensions") {
        let exts = match v.to_str() {
            Ok(s) => s,
            Err(_) => return bad_request("Invalid header"),
        };

        if exts.contains("grip") {
            grip_offered = true;
        }
    }

    if let Some(v) = req.get_header("Connection-Id") {
        cid = match v.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return bad_request("Invalid header"),
        }
    }

    if let Some(v) = req.get_header("Meta-State") {
        match serde_json::from_slice(v.as_bytes()) {
            Ok(v) => state = v,
            Err(_) => return bad_request("Invalid header"),
        }

        connected_subs = state.subs.clone();
    }

    let mut replayed = 0;

    if let Some(v) = req.get_header("Content-Bytes-Replayed") {
        match v.to_str() {
            Ok(s) => match s.parse() {
                Ok(x) => replayed = x,
                Err(_) => return bad_request("Invalid header"),
            },
            Err(_) => return bad_request("Invalid header"),
        }
    }

    println!("{} receiving {} replayed bytes", cid, replayed);

    let mut events = Vec::new();
    let mut pos = 0;

    while pos < body.len() {
        match parse_websocket_event(&body[pos..]) {
            Ok((e, size)) => {
                events.push(e);
                pos += size;
            }
            Err(_) => return bad_request("Failed to parse WebSocket events"),
        }
    }

    let mut ctx = Context {
        handler_ctx: mqtthandler::Context {
            config,
            authorizor,
            disconnect: false,
            state,
        },
        cid,
        in_buf: Vec::new(),
        content_accepted: 0,
    };

    let mut out_events = Vec::new();

    for e in events {
        out_events.extend(handle_websocket_event(&mut ctx, e, |ctx, p| {
            handler(ctx, p)
        }));
    }

    for sub in &ctx.handler_ctx.state.subs {
        if !connected_subs.contains(sub) {
            let cmsg = ControlMessage {
                ctype: "subscribe".to_string(),
                channel: Some(format!("s:{}", sub)),
            };

            out_events.push(WsEvent {
                etype: "TEXT".to_string(),
                content: format!("c:{}", serde_json::to_string(&cmsg).unwrap()).into_bytes(),
            });
        }
    }

    for sub in connected_subs.iter() {
        if !ctx.handler_ctx.state.subs.contains(sub) {
            let cmsg = ControlMessage {
                ctype: "unsubscribe".to_string(),
                channel: Some(format!("s:{}", sub)),
            };

            out_events.push(WsEvent {
                etype: "TEXT".to_string(),
                content: format!("c:{}", serde_json::to_string(&cmsg).unwrap()).into_bytes(),
            });
        }
    }

    if ctx.handler_ctx.disconnect {
        let code: u16 = 1000;

        out_events.push(WsEvent {
            etype: "CLOSE".to_string(),
            content: Vec::from(code.to_be_bytes()),
        });
    }

    let mut body = Vec::new();

    for e in out_events {
        if !e.content.is_empty() {
            write!(&mut body, "{} {:x}\r\n", e.etype, e.content.len()).unwrap();
            body.write_all(&e.content).unwrap();
            body.write_all(b"\r\n").unwrap();
        } else {
            write!(&mut body, "{}\r\n", e.etype).unwrap();
        }
    }

    let mut resp = Response::from_status(StatusCode::OK)
        .with_header("Content-Type", "application/websocket-events")
        .with_body(Body::from(body));

    if grip_offered {
        resp.append_header("Sec-WebSocket-Extensions", "grip");
        resp.append_header("Sec-WebSocket-Protocol", "mqtt");
    }

    println!("{} accepting {} bytes", ctx.cid, ctx.content_accepted);

    resp.append_header("Content-Bytes-Accepted", ctx.content_accepted.to_string());

    let state = serde_json::to_string(&ctx.handler_ctx.state).unwrap();
    resp.append_header("Set-Meta-State", state);

    resp
}

pub fn post(config: &Config, authorizor: &dyn Authorizor, mut req: Request) -> Response {
    let body = req.take_body().into_bytes();

    if req.get_header("Content-Type")
        == Some(&HeaderValue::from_static("application/websocket-events"))
    {
        handle_websocket_events(config, authorizor, req, body, mqtthandler::handle_packet)
    } else {
        Response::from_status(StatusCode::NOT_ACCEPTABLE).with_body_text_plain("Not Acceptable\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TestAuthorizor;
    use crate::config::Config;
    use crate::mqttpacket::Publish;
    use std::borrow::Cow;
    use std::io::Write;

    #[test]
    fn handle_events() {
        let config = Config::default();
        let authorizor = TestAuthorizor;

        let p = Publish {
            topic: Cow::from("fruit"),
            message: Cow::from("apple".as_bytes()),
            dup: false,
            qos: 0,
            retain: false,
            message_expiry_interval: None,
        };

        let mut packet_bytes = Vec::new();
        Packet::Publish(p).serialize(&mut packet_bytes).unwrap();

        let part1 = &packet_bytes[..7];
        let part2 = &packet_bytes[7..];

        let mut body = Vec::new();
        write!(&mut body, "BINARY {:x}\r\n", part1.len()).unwrap();
        body.write_all(&part1).unwrap();
        write!(&mut body, "\r\n").unwrap();

        {
            let req = Request::post("http://localhost/path");

            let mut out = None;
            let resp = handle_websocket_events(&config, &authorizor, req, body.clone(), |_, p| {
                if let Packet::Publish(p) = &p {
                    out = Some(Publish {
                        topic: Cow::from(p.topic.clone().into_owned()),
                        message: Cow::from(p.message.clone().into_owned()),
                        dup: false,
                        qos: 0,
                        retain: false,
                        message_expiry_interval: None,
                    });
                }

                Vec::new()
            });
            assert_eq!(resp.get_status(), StatusCode::OK);
            assert_eq!(resp.get_header_str("Content-Bytes-Accepted"), Some("0"));
            assert!(out.is_none());
        }

        write!(&mut body, "BINARY {:x}\r\n", part2.len()).unwrap();
        body.write_all(&part2).unwrap();
        write!(&mut body, "\r\n").unwrap();

        {
            let req = Request::post("http://localhost/path")
                .with_header("Content-Bytes-Replayed", part1.len().to_string());

            let mut out = None;
            let resp = handle_websocket_events(&config, &authorizor, req, body, |_, p| {
                if let Packet::Publish(p) = &p {
                    out = Some(Publish {
                        topic: Cow::from(p.topic.clone().into_owned()),
                        message: Cow::from(p.message.clone().into_owned()),
                        dup: false,
                        qos: 0,
                        retain: false,
                        message_expiry_interval: None,
                    });
                }

                Vec::new()
            });
            assert_eq!(resp.get_status(), StatusCode::OK);
            assert_eq!(resp.get_header_str("Content-Bytes-Accepted"), Some("15"));
            let out = out.unwrap();
            assert_eq!(out.topic, "fruit");
            assert_eq!(out.message, "apple".as_bytes());
        }
    }
}
