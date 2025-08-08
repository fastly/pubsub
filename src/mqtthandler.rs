use crate::auth::Authorization;
use crate::config::Config;
use crate::mqttpacket::{
    ConnAck, ConnAckV4, Connect, Disconnect, Packet, PingReq, PingResp, Publish, Reason, SubAck,
    Subscribe, UnsubAck, Unsubscribe,
};
use crate::publish::{publish, Sequencing, MESSAGE_SIZE_MAX};
use crate::storage::{RetainedVersion, Storage, StorageError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Not;
use std::time::Duration;

const PACKET_SIZE_MAX: usize = 32_768;

#[derive(Deserialize, Serialize, Default)]
pub struct Version {
    #[serde(rename = "g")]
    pub generation: u64,

    #[serde(rename = "s")]
    pub seq: u64,
}

impl Version {
    pub fn to_id(&self) -> String {
        format!("{:16x}-{}", self.generation, self.seq)
    }
}

#[derive(Deserialize, Serialize, Default)]
pub struct Last {
    #[serde(rename = "v", skip_serializing_if = "Option::is_none")]
    pub version: Option<Version>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct Subscription {
    #[serde(rename = "nl", skip_serializing_if = "<&bool>::not", default)]
    pub no_local: bool,

    #[serde(rename = "rap", skip_serializing_if = "<&bool>::not", default)]
    pub retain_as_published: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<Last>,

    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ignore: Vec<Version>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct State {
    pub connected: bool,
    pub client_id: String,
    pub token: Option<String>,
    pub subs: HashMap<String, Subscription>,
}

impl State {
    fn clear(&mut self) {
        self.connected = false;
        self.client_id.clear();
        self.token = None;
        self.subs.clear();
    }
}

pub struct Context<'a> {
    pub config: &'a Config,
    pub auth: &'a Authorization,
    pub storage: &'a dyn Storage,
    pub disconnect: bool,
    pub state: State,
}

fn handle_connect<'a>(ctx: &mut Context, p: Connect<'a>) -> Vec<Packet<'a>> {
    if p.version != 5 {
        let out = if p.version > 5 {
            Packet::ConnAck(ConnAck {
                reason: Reason::UnsupportedProtocolVersion,
                maximum_packet_size: None,
            })
        } else {
            Packet::ConnAckV4(ConnAckV4 { ret: 0x01 }) // unacceptable protocol version
        };

        ctx.disconnect = true;

        return vec![out];
    }

    if ctx.state.connected {
        return vec![Packet::ConnAck(ConnAck {
            reason: Reason::ProtocolError,
            maximum_packet_size: None,
        })];
    }

    // mark the session as connected and stash the token

    ctx.state.connected = true;
    ctx.state.client_id = p.client_id.to_string();

    if let Some(s) = p.password {
        ctx.state.token = Some(s.to_string());
    }

    vec![Packet::ConnAck(ConnAck {
        reason: Reason::Success,
        maximum_packet_size: Some(PACKET_SIZE_MAX as u32),
    })]
}

fn handle_disconnect(ctx: &mut Context, _p: Disconnect) -> Vec<Packet<'static>> {
    ctx.state.clear();

    vec![]
}

fn handle_pingreq(_ctx: &mut Context, _p: PingReq) -> Vec<Packet<'static>> {
    vec![Packet::PingResp(PingResp)]
}

fn handle_subscribe<'a>(ctx: &mut Context, p: Subscribe<'a>) -> Vec<Packet<'a>> {
    if p.topic.is_empty() {
        return vec![Packet::SubAck(SubAck {
            id: p.id,
            reason: Reason::UnspecifiedError,
        })];
    }

    // reject wildcards, for now
    if p.topic.chars().any(|c| ['#', '+'].contains(&c)) {
        return vec![Packet::SubAck(SubAck {
            id: p.id,
            reason: Reason::WildcardSubscriptionsNotSupported,
        })];
    }

    let mut allowed = false;

    if let Some(s) = &ctx.state.token {
        if let Ok(caps) = ctx.auth.app_token.validate_token(s) {
            if caps.can_subscribe(p.topic) {
                allowed = true;
            }
        }
    }

    if !allowed {
        return vec![Packet::SubAck(SubAck {
            id: p.id,
            reason: Reason::NotAuthorized,
        })];
    }

    let mut retained = None;

    match ctx.storage.read_retained(p.topic, None) {
        Ok(Some(r)) => retained = Some(r),
        Ok(None) | Err(StorageError::StoreNotFound) => {}
        Err(e) => {
            println!("failed to read message from storage: {e:?}");

            return vec![Packet::SubAck(SubAck {
                id: p.id,
                reason: Reason::UnspecifiedError,
            })];
        }
    }

    let version = retained.as_ref().map(|r| Version {
        generation: r.version.generation,
        seq: r.version.seq,
    });

    ctx.state.subs.insert(
        p.topic.to_string(),
        Subscription {
            no_local: p.no_local,
            retain_as_published: p.retain_as_published,
            last: Some(Last { version }),
            ignore: Vec::new(),
        },
    );

    let mut out = vec![Packet::SubAck(SubAck {
        id: p.id,
        reason: Reason::Success,
    })];

    // 0 means send upon new subscription
    if p.retain_handling == 0 {
        if let Some(r) = retained {
            if let Some(message) = r.message {
                out.push(Packet::Publish(Publish {
                    topic: p.topic.into(),
                    message: message.data.into(),
                    dup: false,
                    qos: 0,
                    retain: true,
                    message_expiry_interval: message.ttl.map(|d| d.as_secs() as u32),
                }));
            }
        }
    }

    out
}

fn handle_unsubscribe<'a>(ctx: &mut Context, p: Unsubscribe<'a>) -> Vec<Packet<'a>> {
    let reason = if ctx.state.subs.contains_key(p.topic) {
        ctx.state.subs.remove(p.topic);

        Reason::Success
    } else {
        Reason::NoSubscriptionExisted
    };

    vec![Packet::UnsubAck(UnsubAck { id: p.id, reason })]
}

fn handle_publish<'a>(ctx: &mut Context, p: Publish<'a>) -> Vec<Packet<'a>> {
    if p.topic.starts_with('$') {
        // don't accept publishes to topics beginning with $, per the spec
        return vec![];
    }

    // QoS must be 0
    if p.qos > 0 {
        let out = vec![Packet::Disconnect(Disconnect {
            reason: Reason::QoSNotSupported,
        })];

        ctx.disconnect = true;

        return out;
    }

    let mut allowed = false;

    if let Some(s) = &ctx.state.token {
        if let Ok(caps) = ctx.auth.app_token.validate_token(s) {
            if caps.can_publish(p.topic.as_ref()) {
                allowed = true;
            }
        }
    }

    if !allowed || p.message.len() > MESSAGE_SIZE_MAX {
        return vec![];
    }

    let mut out = vec![];

    let mut version = None;

    if p.retain {
        let ttl = p
            .message_expiry_interval
            .map(|x| Duration::from_secs(x.into()));

        match ctx.storage.write_retained(&p.topic, &p.message, ttl) {
            Ok(v) => version = Some(v),
            Err(e) => {
                // no error response. only log
                println!("failed to write message to storage: {e:?}");
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

    let ignore = match ctx.state.subs.get(&*p.topic) {
        Some(sub) => sub.no_local,
        None => false,
    };

    if !ctx.config.publish_token.is_empty() {
        if let Err(e) = publish(
            &ctx.config.publish_token,
            &p.topic,
            &p.message,
            seq,
            Some(&ctx.state.client_id),
        ) {
            // no error response. only log
            println!("failed to publish: {e:?}");
        }
    } else if seq.is_none() && !ignore {
        println!("publishing not configured, echoing back to sender");
        out.push(Packet::Publish(Publish {
            topic: p.topic,
            message: p.message,
            dup: false,
            qos: 0,
            retain: false,                 // always false for non-durable
            message_expiry_interval: None, // always none for non-durable
        }));
    }

    out
}

pub fn handle_packet<'a>(ctx: &mut Context, p: Packet<'a>) -> Vec<Packet<'a>> {
    let mut out = Vec::new();

    match p {
        Packet::Connect(p) => out.extend(handle_connect(ctx, p)),
        Packet::Disconnect(p) => out.extend(handle_disconnect(ctx, p)),
        Packet::PingReq(p) => out.extend(handle_pingreq(ctx, p)),
        Packet::Subscribe(p) => out.extend(handle_subscribe(ctx, p)),
        Packet::Unsubscribe(p) => out.extend(handle_unsubscribe(ctx, p)),
        Packet::Publish(p) => out.extend(handle_publish(ctx, p)),
        Packet::Unsupported(ptype) => {
            println!("skipping unsupported packet type {ptype}")
        }
        _ => println!("skipping unexpected packet"),
    }

    out
}

pub fn handle_sync(ctx: &mut Context) -> Vec<Packet<'static>> {
    let mut out = Vec::new();

    for (topic, sub) in &mut ctx.state.subs {
        let Some(last) = &mut sub.last else {
            continue;
        };

        let after = last.version.as_ref().map(|v| RetainedVersion {
            generation: v.generation,
            seq: v.seq,
        });

        let r = match ctx.storage.read_retained(topic, after) {
            Ok(Some(r)) => r,
            Ok(None) | Err(StorageError::StoreNotFound) => continue,
            Err(e) => {
                println!("failed to read message from storage: {e:?}");

                out.push(Packet::Disconnect(Disconnect {
                    reason: Reason::UnspecifiedError,
                }));

                ctx.disconnect = true;

                break;
            }
        };

        last.version = Some(Version {
            generation: r.version.generation,
            seq: r.version.seq,
        });

        let mut ignore = false;

        sub.ignore.retain(|i| {
            if r.version.generation == i.generation && r.version.seq == i.seq {
                ignore = true;
            }

            // keep later ignored versions
            i.generation == r.version.generation && i.seq > r.version.seq
        });

        if let Some(message) = r.message {
            if !ignore {
                out.push(Packet::Publish(Publish {
                    topic: topic.to_string().into(),
                    message: message.data.into(),
                    dup: false,
                    qos: 0,
                    retain: sub.retain_as_published,
                    message_expiry_interval: message.ttl.map(|d| d.as_secs() as u32),
                }));
            }
        }
    }

    out
}
