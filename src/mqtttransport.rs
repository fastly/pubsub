use crate::auth::Authorization;
use crate::config::Config;
use crate::grip::ControlMessage;
use crate::mqtthandler;
use crate::mqttpacket::Packet;
use crate::storage::Storage;
use crate::websocket::{parse_websocket_event, WsEvent};
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
    opening: bool,
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
        "OPEN" => {
            ctx.opening = true;

            // ack
            out_events.push(e.clone())
        }
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

fn handle_websocket_events<P, S>(
    config: &Config,
    auth: &Authorization,
    storage: &dyn Storage,
    req: Request,
    body: Vec<u8>,
    mut packet_handler: P,
    mut sync_handler: S,
) -> Response
where
    P: for<'a> FnMut(&mut mqtthandler::Context, Packet<'a>) -> Vec<Packet<'a>>,
    S: FnMut(&mut mqtthandler::Context) -> Vec<Packet<'static>>,
{
    let mut grip_offered = false;
    let mut protocol_requested = false;
    let mut cid = String::new();
    let mut state = mqtthandler::State::default();
    let mut client_id = String::new();
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

    if let Some(v) = req.get_header("Sec-WebSocket-Protocol") {
        let protocols = match v.to_str() {
            Ok(s) => s,
            Err(_) => return bad_request("Invalid header"),
        };

        if protocols.split(" ").any(|s| s == "mqtt") {
            protocol_requested = true;
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
            Err(e) => {
                println!("failed to parse state: {e}");
                return bad_request("Invalid header");
            }
        }

        client_id = state.client_id.clone();
        connected_subs = state.subs.keys().map(|s| s.to_string()).collect();
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

    println!("{cid} receiving {replayed} replayed bytes");

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
            auth,
            storage,
            disconnect: false,
            state,
        },
        cid,
        in_buf: Vec::new(),
        opening: false,
        content_accepted: 0,
    };

    let mut out_events = Vec::new();

    for p in sync_handler(&mut ctx.handler_ctx) {
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

    for e in events {
        out_events.extend(handle_websocket_event(&mut ctx, e, |ctx, p| {
            packet_handler(ctx, p)
        }));
    }

    let mut cmsgs = Vec::new();

    if ctx.handler_ctx.state.client_id != client_id {
        cmsgs.push(ControlMessage {
            ctype: "set-meta".to_string(),
            name: Some("user".to_string()),
            value: Some(ctx.handler_ctx.state.client_id.clone()),
            ..Default::default()
        })
    }

    for (topic, sub) in &ctx.handler_ctx.state.subs {
        if !connected_subs.contains(topic) {
            let mut filters = Vec::new();

            if sub.no_local {
                filters.push("skip-self".to_string());
            }

            cmsgs.push(ControlMessage {
                ctype: "subscribe".to_string(),
                channel: Some(format!("s:{topic}")),
                filters,
                ..Default::default()
            });

            cmsgs.push(ControlMessage {
                ctype: "subscribe".to_string(),
                channel: Some(format!("d:{topic}")),
                ..Default::default()
            });
        }
    }

    for topic in connected_subs.iter() {
        if !ctx.handler_ctx.state.subs.contains_key(topic.as_str()) {
            cmsgs.push(ControlMessage {
                ctype: "unsubscribe".to_string(),
                channel: Some(format!("s:{topic}")),
                ..Default::default()
            });

            cmsgs.push(ControlMessage {
                ctype: "unsubscribe".to_string(),
                channel: Some(format!("d:{topic}")),
                ..Default::default()
            });
        }
    }

    for cmsg in cmsgs {
        out_events.push(WsEvent {
            etype: "TEXT".to_string(),
            content: format!("c:{}", serde_json::to_string(&cmsg).unwrap()).into_bytes(),
        });
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

    if ctx.opening {
        if grip_offered {
            resp.append_header("Sec-WebSocket-Extensions", "grip");
        }

        if protocol_requested {
            resp.append_header("Sec-WebSocket-Protocol", "mqtt");
        }
    }

    println!("{} accepting {} bytes", ctx.cid, ctx.content_accepted);

    resp.append_header("Content-Bytes-Accepted", ctx.content_accepted.to_string());

    let state = serde_json::to_string(&ctx.handler_ctx.state).unwrap();
    println!("saving state: {state}");
    resp.append_header("Set-Meta-State", state);

    resp.append_header("Keep-Alive-Interval", "120");

    resp
}

pub fn post(
    config: &Config,
    auth: &Authorization,
    storage: &dyn Storage,
    mut req: Request,
) -> Response {
    let body = req.take_body().into_bytes();

    if req.get_header("Content-Type")
        == Some(&HeaderValue::from_static("application/websocket-events"))
    {
        handle_websocket_events(
            config,
            auth,
            storage,
            req,
            body,
            mqtthandler::handle_packet,
            mqtthandler::handle_sync,
        )
    } else {
        Response::from_status(StatusCode::NOT_ACCEPTABLE).with_body_text_plain("Not Acceptable\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Authorization, TestAppTokenAuthorizor, TestGripAuthorizor};
    use crate::config::Config;
    use crate::mqttpacket::Publish;
    use crate::storage::{RetainedSlot, RetainedVersion, StorageError};
    use std::borrow::Cow;
    use std::io::Write;
    use std::time::Duration;

    struct TestStorage;

    impl Storage for TestStorage {
        fn write_retained(
            &self,
            _topic: &str,
            _message: &[u8],
            _ttl: Option<Duration>,
        ) -> Result<RetainedVersion, StorageError> {
            Ok(RetainedVersion {
                generation: 1,
                seq: 1,
            })
        }

        fn read_retained(
            &self,
            _topic: &str,
            _after: Option<RetainedVersion>,
        ) -> Result<Option<RetainedSlot>, StorageError> {
            Ok(None)
        }
    }

    #[test]
    fn handle_events() {
        let config = Config::default();
        let auth = Authorization {
            grip: Box::new(TestGripAuthorizor),
            fastly: false,
            app_token: Box::new(TestAppTokenAuthorizor),
        };
        let storage = TestStorage;

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
            let resp = handle_websocket_events(
                &config,
                &auth,
                &storage,
                req,
                body.clone(),
                |_, p| {
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
                },
                |_| Vec::new(),
            );
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
            let resp = handle_websocket_events(
                &config,
                &auth,
                &storage,
                req,
                body,
                |_, p| {
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
                },
                |_| Vec::new(),
            );
            assert_eq!(resp.get_status(), StatusCode::OK);
            assert_eq!(resp.get_header_str("Content-Bytes-Accepted"), Some("15"));
            let out = out.unwrap();
            assert_eq!(out.topic, "fruit");
            assert_eq!(out.message, "apple".as_bytes());
        }
    }
}
