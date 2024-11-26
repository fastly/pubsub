use crate::auth::Authorizor;
use crate::config::Config;
use crate::mqttpacket::{
    ConnAck, ConnAckV4, Connect, Disconnect, Packet, PingReq, PingResp, Publish, Reason, SubAck,
    Subscribe, UnsubAck, Unsubscribe,
};
use crate::publish::{publish, MESSAGE_SIZE_MAX};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Deserialize, Serialize, Default)]
pub struct State {
    pub connected: bool,
    pub token: Option<String>,
    pub subs: HashSet<String>,
}

impl State {
    fn clear(&mut self) {
        self.connected = false;
        self.token = None;
        self.subs.clear();
    }
}

pub struct Context<'a> {
    pub config: &'a Config,
    pub authorizor: &'a dyn Authorizor,
    pub disconnect: bool,
    pub state: State,
}

pub fn handle_connect(ctx: &mut Context, p: Connect) -> Vec<Packet<'static>> {
    if p.version != 5 {
        let out = if p.version > 5 {
            Packet::ConnAck(ConnAck {
                reason: Reason::UnsupportedProtocolVersion,
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
        })];
    }

    // mark the session as connected and stash the token

    ctx.state.connected = true;

    if let Some(s) = p.password {
        ctx.state.token = Some(s.to_string());
    }

    vec![Packet::ConnAck(ConnAck {
        reason: Reason::Success,
    })]
}

pub fn handle_disconnect(ctx: &mut Context, _p: Disconnect) -> Vec<Packet<'static>> {
    ctx.state.clear();

    vec![]
}

pub fn handle_pingreq(_ctx: &mut Context, _p: PingReq) -> Vec<Packet<'static>> {
    vec![Packet::PingResp(PingResp)]
}

pub fn handle_subscribe(ctx: &mut Context, p: Subscribe) -> Vec<Packet<'static>> {
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

    let reason = if allowed {
        ctx.state.subs.insert(p.topic.to_owned());

        Reason::Success
    } else {
        Reason::NotAuthorized
    };

    vec![Packet::SubAck(SubAck { id: p.id, reason })]
}

pub fn handle_unsubscribe(ctx: &mut Context, p: Unsubscribe) -> Vec<Packet<'static>> {
    let reason = if ctx.state.subs.contains(p.topic) {
        ctx.state.subs.remove(p.topic);

        Reason::Success
    } else {
        Reason::NoSubscriptionExisted
    };

    vec![Packet::UnsubAck(UnsubAck { id: p.id, reason })]
}

pub fn handle_publish(ctx: &mut Context, p: Publish) -> Vec<Packet<'static>> {
    if p.topic.starts_with('$') {
        // don't accept publishes to topics beginning with $, per the spec
        return vec![];
    }

    let mut allowed = false;

    if let Some(s) = &ctx.state.token {
        if let Ok(caps) = ctx.authorizor.validate_token(s) {
            if caps.can_publish(p.topic) {
                allowed = true;
            }
        }
    }

    if allowed && p.message.len() < MESSAGE_SIZE_MAX {
        if let Err(e) = publish(&ctx.config.publish_token, p.topic, p.message) {
            // no error response. only log
            println!("failed to publish: {:?}", e);
        }
    }

    vec![]
}

pub fn handle_packet(ctx: &mut Context, p: Packet) -> Vec<Packet<'static>> {
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
