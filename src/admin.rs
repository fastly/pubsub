use crate::auth::Authorization;
use fastly::http::StatusCode;
use fastly::kv_store;
use fastly::{Request, Response};
use jwt_simple::prelude::*;
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::fmt::Write;

#[derive(Serialize)]
struct Key {
    id: String,
    value: String,
}

fn text_response(status: StatusCode, text: &str) -> Response {
    Response::from_status(status).with_body_text_plain(&format!("{text}\n"))
}

pub fn post_keys(auth: &Authorization, _req: Request) -> Response {
    if !auth.fastly {
        return text_response(
            StatusCode::UNAUTHORIZED,
            "Fastly-Key header invalid or not specified",
        );
    }

    let store = match kv_store::KVStore::open("keys") {
        Ok(Some(store)) => store,
        Ok(None) => {
            println!("kv store not found");

            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Storage access process failed",
            );
        }
        Err(e) => {
            println!("failed to open kv store: {e}");

            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Storage access process failed",
            );
        }
    };

    let key = {
        let random_bytes = HS256Key::generate().to_bytes();

        let mut value = String::new();
        for &b in Sha1::digest(&random_bytes).as_slice() {
            value.write_fmt(format_args!("{b:02x}")).unwrap();
        }

        let mut id = String::new();
        for &b in Sha1::digest(&value).as_slice()[..4].iter() {
            id.write_fmt(format_args!("{b:02x}")).unwrap();
        }

        Key { id, value }
    };

    if let Err(e) = store.insert(&key.id, key.value.clone()) {
        println!("failed to write to kv store: {e}");

        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Storage writing process failed",
        );
    }

    Response::from_status(StatusCode::OK)
        .with_body_json(&key)
        .unwrap()
}
