use crate::mqttpacket::{Packet, Publish};
use base64::Engine;
use fastly::error::anyhow;
use fastly::http::{header, StatusCode};
use fastly::{Error, Request};
use std::env;
use std::fmt::Write;
use std::str;

// allow 256 bytes of protocol overhead
pub const MESSAGE_SIZE_MAX: usize = 32_768 - 256;

pub fn publish(
    api_token: &str,
    topic: &str,
    message: &[u8],
    sender: Option<&str>,
) -> Result<(), Error> {
    let service_id = env::var("FASTLY_SERVICE_ID").unwrap();

    let sse_content = match str::from_utf8(message) {
        Ok(s) => {
            let mut content = String::new();
            content.push_str("event: message\n");

            for line in s.split('\n') {
                content.write_fmt(format_args!("data: {line}\n")).unwrap();
            }

            content.push('\n');

            content
        }
        Err(_) => {
            let encoded = base64::prelude::BASE64_STANDARD.encode(message);

            let mut content = String::new();
            content.push_str("event: message-base64\ndata: ");
            content.push_str(&encoded);
            content.push_str("\n\n");

            content
        }
    };

    let mqtt_content = {
        let mut v = Vec::new();
        Packet::Publish(Publish {
            topic: topic.into(),
            message: message.into(),
            dup: false,
            qos: 0,
            retain: false,
            message_expiry_interval: None,
        })
        .serialize(&mut v)?;

        base64::prelude::BASE64_STANDARD.encode(v)
    };

    let mut item = serde_json::json!({
        "channel": format!("s:{topic}"),
        "formats": {
            "http-stream": {
                "content": sse_content,
            },
            "ws-message": {
                "content-bin": mqtt_content,
            },
        },
    });

    if let Some(sender) = sender {
        item["meta"] = serde_json::json!({
            "sender": sender,
        });
    }

    let body = serde_json::json!({
        "items": [item],
    });

    let body = body.to_string();

    let req = Request::post(format!(
        "https://api.fastly.com/service/{service_id}/publish/"
    ))
    .with_header(header::AUTHORIZATION, format!("Bearer {}", api_token))
    .with_body(body)
    .with_pass(true);

    let resp = req.send("api")?;

    if resp.get_status() != StatusCode::OK {
        let body = resp.into_body().into_bytes();
        return Err(anyhow!(
            "publish error: {:?}",
            String::from_utf8_lossy(&body)
        ));
    }

    Ok(())
}
