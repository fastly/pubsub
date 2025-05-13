use crate::auth::Authorizor;
use crate::config::Config;
use crate::mqttpacket::{
    ConnAck, ConnAckV4, Connect, Disconnect, Packet, PingReq, PingResp, Publish, Reason, SubAck,
    Subscribe, UnsubAck, Unsubscribe,
};
use crate::publish::{publish, MESSAGE_SIZE_MAX};
use crate::storage::Storage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Not;

const PACKET_SIZE_MAX: usize = 32_768;

#[derive(Deserialize, Serialize, Default)]
pub struct Subscription {
    #[serde(rename = "nl", skip_serializing_if = "<&bool>::not", default)]
    pub no_local: bool,
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
    pub authorizor: &'a dyn Authorizor,
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
        if let Ok(caps) = ctx.authorizor.validate_token(s) {
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

    ctx.state.subs.insert(
        p.topic.to_string(),
        Subscription {
            no_local: p.no_local,
        },
    );

    vec![Packet::SubAck(SubAck {
        id: p.id,
        reason: Reason::Success,
    })]
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
        if let Ok(caps) = ctx.authorizor.validate_token(s) {
            if caps.can_publish(p.topic.as_ref()) {
                allowed = true;
            }
        }
    }

    if !allowed || p.message.len() > MESSAGE_SIZE_MAX {
        return vec![];
    }

    let mut out = vec![];

    let ignore = match ctx.state.subs.get(&*p.topic) {
        Some(sub) => sub.no_local,
        None => false,
    };

    if !ctx.config.publish_token.is_empty() {
        if let Err(e) = publish(
            &ctx.config.publish_token,
            &p.topic,
            &p.message,
            Some(&ctx.state.client_id),
        ) {
            // no error response. only log
            println!("failed to publish: {:?}", e);
        }
    } else if !ignore {
        println!("publishing not configured, echoing back to sender");
        out.push(Packet::Publish(Publish {
            topic: p.topic,
            message: p.message,
            dup: false,
            qos: 0,
            retain: false,
            message_expiry_interval: None,
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
            println!("skipping unsupported packet type {}", ptype)
        }
        _ => println!("skipping unexpected packet"),
    }

    out
}
