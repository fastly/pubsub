use crate::auth::Authorizor;
use crate::config::Config;
use crate::mqtthandler;
use crate::mqttpacket::Packet;
use crate::websocket::{parse_websocket_event, ControlMessage, WsEvent};
use fastly::http::{HeaderValue, StatusCode};
use fastly::{Body, Request, Response};
use std::collections::HashSet;
use std::io::{self, Write};
use std::mem;
use std::str;

struct Context<'a> {
    handler_ctx: mqtthandler::Context<'a>,
    cid: String,
    in_buf: Vec<u8>,
}

fn handle_websocket_event(ctx: &mut Context, e: WsEvent) -> Vec<WsEvent> {
    let mut out_events = Vec::new();

    println!("{} event {} size={}", ctx.cid, e.etype, e.content.len());

    match e.etype.as_str() {
        "OPEN" => out_events.push(e.clone()),  // ack
        "CLOSE" => out_events.push(e.clone()), // ack
        "TEXT" | "BINARY" => {
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

                for p in mqtthandler::handle_packet(&mut ctx.handler_ctx, p) {
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
            }

            ctx.in_buf = in_buf;
        }
        _ => {} // unsupported event type, ignore
    }

    out_events
}

fn bad_request<T: AsRef<str>>(message: T) -> Response {
    Response::from_status(400).with_body_text_plain(&format!("{}\n", message.as_ref()))
}

fn escape_header(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();

    for &b in data {
        if b < 0x20 || b == b'\\' || b == b',' || b == 0x7f {
            write!(&mut out, "\\x{:02x}", b).unwrap();
        } else {
            out.push(b);
        }
    }

    out
}

fn unescape_header(value: &[u8]) -> Result<Vec<u8>, io::Error> {
    let mut out = Vec::new();

    let mut pos = 0;

    while pos < value.len() {
        let b = value[pos];

        if b == b'\\' {
            pos += 1;

            if value[pos] != b'x' || pos + 2 >= value.len() {
                return Err(io::ErrorKind::InvalidData.into());
            }

            let Ok(s) = str::from_utf8(&value[(pos + 1)..(pos + 3)]) else {
                return Err(io::ErrorKind::InvalidData.into());
            };

            let Ok(v) = u8::from_str_radix(s, 16) else {
                return Err(io::ErrorKind::InvalidData.into());
            };

            out.push(v);
            pos += 3;
        } else {
            out.push(b);
            pos += 1;
        }
    }

    Ok(out)
}

fn handle_websocket_events(
    config: &Config,
    authorizor: &dyn Authorizor,
    req: Request,
    body: Vec<u8>,
) -> Response {
    let mut grip_offered = false;
    let mut cid = String::new();
    let mut in_buf = Vec::new();
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

    if let Some(v) = req.get_header("Meta-Buf") {
        match unescape_header(v.as_bytes()) {
            Ok(v) => in_buf.extend(v),
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

    println!("{} using {} saved bytes", cid, in_buf.len());

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
        in_buf,
    };

    let mut out_events = Vec::new();

    for e in events {
        out_events.extend(handle_websocket_event(&mut ctx, e));
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

    println!("{} saving {} bytes", ctx.cid, ctx.in_buf.len());

    resp.append_header("Set-Meta-Buf", escape_header(&ctx.in_buf));

    let state = serde_json::to_string(&ctx.handler_ctx.state).unwrap();
    resp.append_header("Set-Meta-State", state);

    resp
}

pub fn post(config: &Config, authorizor: &dyn Authorizor, mut req: Request) -> Response {
    let body = req.take_body().into_bytes();

    if req.get_header("Content-Type")
        == Some(&HeaderValue::from_static("application/websocket-events"))
    {
        handle_websocket_events(config, authorizor, req, body)
    } else {
        Response::from_status(StatusCode::NOT_ACCEPTABLE).with_body_text_plain("Not Acceptable\n")
    }
}
