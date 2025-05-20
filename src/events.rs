use crate::auth::{AuthorizationError, Authorizor, Capabilities};
use crate::config::Config;
use crate::publish::{publish, Sequencing, MESSAGE_SIZE_MAX};
use crate::storage::Storage;
use fastly::http::{header, StatusCode};
use fastly::{Request, Response};
use std::collections::{HashMap, HashSet};
use std::str;
use std::time::Duration;

const TOPICS_PER_REQUEST_MAX: usize = 10;

struct Version {
    generation: u64,
    seq: u64,
}

impl Version {
    fn to_id(&self) -> String {
        format!("{}-{}", self.generation, self.seq)
    }
}

fn text_response(status: StatusCode, text: &str) -> Response {
    Response::from_status(status).with_body_text_plain(&format!("{text}\n"))
}

fn sse_error(condition: &str, text: &str) -> Response {
    let mut data = HashMap::new();

    data.insert("condition".to_string(), condition.to_string());
    data.insert("text".to_string(), text.to_string());

    let data = serde_json::to_string(&data).unwrap();

    Response::from_status(StatusCode::OK)
        .with_header(header::CONTENT_TYPE, "text/event-stream")
        .with_body(format!("event: stream-error\ndata: {data}\n\n"))
}

pub fn get(authorizor: &dyn Authorizor, req: Request) -> Response {
    let topics = {
        let mut topics = HashSet::new();

        for (k, v) in req.get_url().query_pairs() {
            if k == "topic" {
                topics.insert(v.to_string());
            }
        }

        topics
    };

    if topics.is_empty() {
        return sse_error("bad-request", "Missing 'topic' parameter");
    }

    if topics.len() >= TOPICS_PER_REQUEST_MAX {
        return sse_error("bad-request", "Too many topics");
    }

    let caps = if req.fastly_key_is_valid() {
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
                    &format!("Unsupported authorization scheme: {}", scheme),
                );
            }

            value
        } else {
            return sse_error(
                "bad-request",
                "Missing 'Authorization' header or 'auth' parameter",
            );
        };

        match authorizor.validate_token(token, None) {
            Ok(caps) => caps,
            Err(AuthorizationError::Token(_)) => {
                return sse_error("forbidden", "Invalid token");
            }
            Err(e) => {
                println!("auth failed: {:?}", e);

                return sse_error("internal-server-error", "Auth process failed");
            }
        }
    };

    for topic in &topics {
        if !caps.can_subscribe(topic) {
            return sse_error("forbidden", &format!("Cannot subscribe to topic: {topic}"));
        }
    }

    let mut resp = Response::from_status(StatusCode::OK)
        .with_header(header::CONTENT_TYPE, "text/event-stream")
        .with_header("Grip-Hold", "stream")
        .with_header(
            "Grip-Keep-Alive",
            "event: keep-alive\\ndata: \\n\\n; format=cstring; timeout=55",
        );

    for topic in topics {
        resp.append_header("Grip-Channel", format!("s:{topic}"));
    }

    resp.with_body("event: stream-open\ndata: \n\n")
}

pub fn post(
    config: &Config,
    authorizor: &dyn Authorizor,
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

    let caps = if req.fastly_key_is_valid() {
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

        match authorizor.validate_token(token, None) {
            Ok(caps) => caps,
            Err(AuthorizationError::Token(_)) => {
                return text_response(StatusCode::FORBIDDEN, "Invalid token");
            }
            Err(e) => {
                println!("auth failed: {:?}", e);

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
                println!("failed to write message to storage: {:?}", e);

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
            .to_id()
        } else {
            // if we wrote version 1, it implies the slot was empty
            "none".to_string()
        };

        Sequencing {
            id: version.to_id(),
            prev_id,
        }
    });

    if let Err(e) = publish(&config.publish_token, topic, &message, seq, None) {
        println!("failed to publish: {:?}", e);

        return text_response(StatusCode::INTERNAL_SERVER_ERROR, "Publish process failed");
    }

    text_response(StatusCode::OK, "Published")
}
