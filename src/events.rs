use crate::auth::{Authorization, AuthorizationError, Capabilities};
use crate::config::Config;
use crate::publish::{publish, Sequencing, MESSAGE_SIZE_MAX};
use crate::storage::{RetainedVersion, Storage, StorageError};
use base64::Engine;
use fastly::http::{header, StatusCode};
use fastly::{Request, Response};
use std::collections::HashMap;
use std::fmt::Write;
use std::str;
use std::time::Duration;
use thiserror::Error;

const TOPICS_PER_REQUEST_MAX: usize = 10;
const NEXT_TIMEOUT_SECS: usize = 120;

struct VersionParseError;

#[derive(Debug, Copy, Clone)]
struct Version {
    generation: u64,
    seq: u64,
}

impl Version {
    fn as_id(&self) -> String {
        format!("{:16x}-{}", self.generation, self.seq)
    }

    fn parse(s: &str) -> Result<Self, VersionParseError> {
        let pos = match s.find('-') {
            Some(pos) => pos,
            None => return Err(VersionParseError),
        };

        let generation = &s[..pos];
        let seq = &s[(pos + 1)..];

        let Ok(generation) = u64::from_str_radix(generation, 16) else {
            return Err(VersionParseError);
        };

        let Ok(seq) = seq.parse() else {
            return Err(VersionParseError);
        };

        Ok(Self { generation, seq })
    }
}

#[derive(Error, Debug)]
enum GripLastError<'a> {
    #[error("invalid header: [{0}]")]
    ParseHeader(&'a str),
}

// if there is at least one Grip-Last header, this function is guaranteed
// to return at least one item or error
fn parse_grip_last(req: &Request) -> Result<Vec<(&str, &str)>, GripLastError> {
    let mut out = Vec::new();

    for hvalue in req.get_header_all_str("Grip-Last") {
        for value in hvalue.split(',') {
            let Some(pos) = value.find(';') else {
                return Err(GripLastError::ParseHeader(hvalue));
            };

            let channel = value[..pos].trim();
            let params = &value[(pos + 1)..];

            let Some(pos) = params.find("last-id=") else {
                return Err(GripLastError::ParseHeader(hvalue));
            };

            let remainder = &params[(pos + 8)..];

            let end = match remainder.find(';') {
                Some(pos) => pos,
                None => remainder.len(),
            };

            let id = remainder[..end].trim();

            out.push((channel, id));
        }
    }

    Ok(out)
}

fn text_response(status: StatusCode, text: &str) -> Response {
    Response::from_status(status).with_body_text_plain(&format!("{text}\n"))
}

fn sse_error(condition: &str, text: &str) -> Response {
    let mut data = HashMap::new();

    data.insert("condition".to_string(), condition.to_string());
    data.insert("text".to_string(), text.to_string());

    let data = serde_json::to_string(&data).unwrap();

    Response::new()
        .with_header(header::CONTENT_TYPE, "text/event-stream")
        .with_body(format!("event: stream-error\ndata: {data}\n\n"))
}

pub fn get(auth: &Authorization, storage: &dyn Storage, req: Request) -> Response {
    let grip_last = match parse_grip_last(&req) {
        Ok(v) => v,
        Err(e) => {
            println!("failed to parse Grip-Last: {e}");

            // close (200 w/o grip instructions when stream is open means close)
            return Response::new();
        }
    };

    let is_next = !grip_last.is_empty();

    let mut topics = HashMap::new();

    if is_next {
        for &(channel, last_id) in &grip_last {
            if !channel.starts_with("d:") {
                continue;
            }

            let topic = &channel[2..];

            let version = if last_id != "none" {
                let Ok(version) = Version::parse(last_id) else {
                    println!("grip last ID not a valid version: [last_id]");

                    // close (200 w/o grip instructions when stream is open means close)
                    return Response::new();
                };

                Some(version)
            } else {
                None
            };

            topics.insert(topic.to_string(), version);
        }

        if topics.is_empty() {
            println!("no valid grip last topics");

            // close (200 w/o grip instructions when stream is open means close)
            return Response::new();
        }
    } else {
        for (k, v) in req.get_url().query_pairs() {
            if k == "topic" {
                topics.insert(v.to_string(), None);
            }
        }

        if topics.is_empty() {
            return sse_error("bad-request", "Missing 'topic' parameter");
        }
    }

    if topics.len() >= TOPICS_PER_REQUEST_MAX {
        return sse_error("bad-request", "Too many topics");
    }

    if !is_next {
        let last_event_id = if let Some(s) = req.get_query_parameter("lastEventId") {
            Some(s)
        } else {
            req.get_header_str("Last-Event-ID")
        };

        if let Some(last_event_id) = last_event_id {
            for part in last_event_id.split(',') {
                let Some(pos) = part.find(':') else {
                    return sse_error("bad-request", "Last-Event-ID part missing ':'\n");
                };

                let topic = &part[..pos];
                let version = &part[(pos + 1)..];

                let Ok(version) = Version::parse(version) else {
                    return sse_error(
                        "bad-request",
                        &format!("Last-Event-ID part not a valid version: [{version}]\n"),
                    );
                };

                if let Some(v) = topics.get_mut(topic) {
                    *v = Some(version);
                }
            }
        }
    }

    let durable = req.get_query_parameter("durable") == Some("true");

    let caps = if is_next || auth.fastly {
        Capabilities::new_admin()
    } else {
        let token = if let Some(v) = req.get_query_parameter("auth") {
            v
        } else if let Some(v) = req.get_header_str(header::AUTHORIZATION) {
            let pos = match v.find(' ') {
                Some(pos) => pos,
                None => return sse_error("bad-request", "Invalid 'Authorization' header"),
            };

            let scheme = &v[..pos];
            let value = &v[(pos + 1)..];

            if scheme != "Bearer" {
                return sse_error(
                    "bad-request",
                    &format!("Unsupported authorization scheme: {scheme}"),
                );
            }

            value
        } else {
            return sse_error(
                "bad-request",
                "Missing 'Authorization' header or 'auth' parameter",
            );
        };

        let caps = match auth.app_token.validate_token(token) {
            Ok(caps) => caps,
            Err(AuthorizationError::Token(_)) => {
                return sse_error("forbidden", "Invalid token");
            }
            Err(e) => {
                println!("auth failed: {e:?}");

                return sse_error("internal-server-error", "Auth process failed");
            }
        };

        caps
    };

    for topic in topics.keys() {
        if !caps.can_subscribe(topic) {
            return sse_error("forbidden", &format!("Cannot subscribe to topic: {topic}"));
        }
    }

    let mut events = Vec::new();

    if durable {
        let mut keys: Vec<String> = topics.keys().cloned().collect();
        keys.sort();

        for topic in &keys {
            let version = topics.get_mut(topic).unwrap();

            let after = version.map(|v| RetainedVersion {
                generation: v.generation,
                seq: v.seq,
            });

            let retained = match storage.read_retained(topic, after) {
                Ok(Some(r)) => r,
                Ok(None) | Err(StorageError::StoreNotFound) => continue,
                Err(e) => {
                    println!("failed to read message from storage: {e:?}");

                    return sse_error(
                        "internal-server-error",
                        "Failed to read message from storage",
                    );
                }
            };

            let v = Version {
                generation: retained.version.generation,
                seq: retained.version.seq,
            };

            *version = Some(v);

            let Some(message) = retained.message else {
                continue;
            };

            let id = {
                let mut parts = Vec::new();

                for topic in &keys {
                    if let Some(v) = &topics[topic] {
                        let id = v.as_id();
                        parts.push(format!("{topic}:{id}"));
                    }
                }

                parts.join(",")
            };

            let sse_content = match str::from_utf8(&message.data) {
                Ok(s) => {
                    let mut content = String::new();
                    content.push_str("event: message\n");
                    content.write_fmt(format_args!("id: {id}\n")).unwrap();

                    for line in s.split('\n') {
                        content.write_fmt(format_args!("data: {line}\n")).unwrap();
                    }

                    content.push('\n');

                    content
                }
                Err(_) => {
                    let encoded = base64::prelude::BASE64_STANDARD.encode(message.data);

                    let mut content = String::new();
                    content.push_str("event: message-base64\n");
                    content.write_fmt(format_args!("id: {id}\n")).unwrap();
                    content.push_str("data: ");
                    content.push_str(&encoded);
                    content.push_str("\n\n");

                    content
                }
            };

            events.push(sse_content);
        }
    }

    let mut resp = Response::new()
        .with_header(header::CONTENT_TYPE, "text/event-stream")
        .with_header("Grip-Hold", "stream")
        .with_header(
            "Grip-Keep-Alive",
            "event: keep-alive\\ndata: \\n\\n; format=cstring; timeout=55",
        );

    for (topic, version) in &topics {
        resp.append_header("Grip-Channel", format!("s:{topic}"));

        if durable {
            let prev_id = match version {
                Some(v) => v.as_id(),
                None => "none".to_string(),
            };

            resp.append_header("Grip-Channel", format!("d:{topic}; prev-id={prev_id}"));
        }
    }

    if durable {
        resp.append_header(
            "Grip-Link",
            format!("</events?durable=true>; rel=next; timeout={NEXT_TIMEOUT_SECS}"),
        );
    }

    let mut body = String::new();

    if !is_next {
        body.push_str("event: stream-open\ndata: \n\n");
    }

    for s in events {
        body.push_str(&s);
    }

    resp.with_body(body)
}

pub fn post(
    config: &Config,
    auth: &Authorization,
    storage: &dyn Storage,
    mut req: Request,
) -> Response {
    let body = req.take_body();

    let Some(topic) = req.get_query_parameter("topic") else {
        return text_response(StatusCode::BAD_REQUEST, "Missing 'topic' param");
    };

    let retain = req.get_query_parameter("retain") == Some("true");

    let ttl: Option<Duration> = match req.get_query_parameter("ttl") {
        Some(x) => match x.parse::<u32>() {
            Ok(x) => Some(Duration::from_secs(x.into())),
            Err(e) => {
                return text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid 'ttl' param: {e}"),
                )
            }
        },
        None => None,
    };

    let caps = if auth.fastly {
        Capabilities::new_admin()
    } else {
        let token = if let Some(v) = req.get_header_str(header::AUTHORIZATION) {
            let pos = match v.find(' ') {
                Some(pos) => pos,
                None => {
                    return text_response(StatusCode::BAD_REQUEST, "Invalid 'Authorization' header")
                }
            };

            let scheme = &v[..pos];
            let value = &v[(pos + 1)..];

            if scheme != "Bearer" {
                return text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("Unsupported authorization scheme: {scheme}"),
                );
            }

            value
        } else {
            return text_response(StatusCode::BAD_REQUEST, "Missing 'Authorization' header");
        };

        match auth.app_token.validate_token(token) {
            Ok(caps) => caps,
            Err(AuthorizationError::Token(_)) => {
                return text_response(StatusCode::FORBIDDEN, "Invalid token");
            }
            Err(e) => {
                println!("auth failed: {e:?}");

                return text_response(StatusCode::INTERNAL_SERVER_ERROR, "Auth process failed");
            }
        }
    };

    if !caps.can_publish(topic) {
        return text_response(
            StatusCode::FORBIDDEN,
            &format!("Cannot publish to topic: {topic}"),
        );
    }

    let message = body.into_bytes();

    if message.len() > MESSAGE_SIZE_MAX {
        return text_response(
            StatusCode::BAD_REQUEST,
            &format!("Message size exceeds {MESSAGE_SIZE_MAX} bytes maximum"),
        );
    }

    let mut version = None;

    if retain {
        match storage.write_retained(topic, &message, ttl) {
            Ok(v) => version = Some(v),
            Err(e) => {
                println!("failed to write message to storage: {e:?}");

                return text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to write message to storage",
                );
            }
        }
    }

    let seq = version.map(|v| {
        let version = Version {
            generation: v.generation,
            seq: v.seq,
        };

        let prev_id = if v.seq > 1 {
            // if we wrote version 2 or later, it implies the slot
            // existed and thus the previous write would have been
            // for the same generation
            Version {
                generation: v.generation,
                seq: v.seq - 1,
            }
            .as_id()
        } else {
            // if we wrote version 1, it implies the slot was empty
            "none".to_string()
        };

        Sequencing {
            id: version.as_id(),
            prev_id,
        }
    });

    if let Err(e) = publish(&config.publish_token, topic, &message, seq, None) {
        println!("failed to publish: {e:?}");

        return text_response(StatusCode::INTERNAL_SERVER_ERROR, "Publish process failed");
    }

    text_response(StatusCode::OK, "Published")
}
