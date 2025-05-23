use crate::{admin, auth, config, events, mqtttransport};
use fastly::http::{header, Method, StatusCode};
use fastly::{Error, Request, Response};

trait WithCors {
    fn with_cors(self) -> Self;
}

impl WithCors for Response {
    fn with_cors(self) -> Self {
        self.with_header("Access-Control-Allow-Origin", "*")
            .with_header(
                "Access-Control-Allow-Methods",
                "OPTIONS, HEAD, GET, POST, PUT, DELETE",
            )
            .with_header(
                "Access-Control-Allow-Headers",
                "Authorization, Content-Type",
            )
            .with_header("Access-Control-Allow-Credentials", "true")
            .with_header("Access-Control-Max-Age", "3600")
    }
}

pub fn handle_request(
    config_source: &dyn config::Source,
    authorizor: &dyn auth::Authorizor,
    req: Request,
) -> Result<(), Error> {
    let config = match config_source.config() {
        Ok(config) => config,
        Err(_) => {
            let resp = Response::from_status(StatusCode::INTERNAL_SERVER_ERROR)
                .with_body_text_plain("Configuration process failed.\n")
                .with_cors();

            resp.send_to_client();

            return Ok(());
        }
    };

    let path = req.get_url().path();

    let resp = if path == "/" {
        Response::from_status(StatusCode::OK).with_body_text_plain("Hello from Fastly Pub/Sub!\n")
    } else if path == "/events" && (config.sse_enabled || config.http_publish_enabled) {
        if req.get_method() == Method::OPTIONS {
            Response::from_status(StatusCode::OK)
        } else if req.get_method() == Method::GET && config.sse_enabled {
            // handoff if necessary
            if req.get_header_str("Grip-Sig").is_none() {
                req.handoff_fanout("self")?;
                return Ok(());
            }

            events::get(authorizor, req)
        } else if req.get_method() == Method::POST && config.http_publish_enabled {
            events::post(&config, authorizor, req)
        } else {
            let mut allow = "OPTIONS".to_string();

            if config.sse_enabled {
                allow.push_str(", GET");
            }

            if config.http_publish_enabled {
                allow.push_str(", POST");
            }

            Response::from_status(StatusCode::METHOD_NOT_ALLOWED)
                .with_header(header::ALLOW, allow)
                .with_body_text_plain("Method Not Allowed\n")
        }
    } else if path == "/mqtt" && config.mqtt_enabled {
        // handoff if necessary
        if req.get_header_str("Grip-Sig").is_none() {
            req.handoff_fanout("self")?;
            return Ok(());
        }

        if req.get_method() == Method::POST {
            mqtttransport::post(&config, authorizor, req)
        } else {
            Response::from_status(StatusCode::METHOD_NOT_ALLOWED)
                .with_header(header::ALLOW, "POST")
                .with_body_text_plain("Method Not Allowed\n")
        }
    } else if path == "/admin/keys" && config.admin_enabled {
        if req.get_method() == "POST" {
            admin::post_keys(req)
        } else {
            Response::from_status(StatusCode::METHOD_NOT_ALLOWED)
                .with_header(header::ALLOW, "POST")
                .with_body_text_plain("Method Not Allowed\n")
        }
    } else {
        Response::from_status(StatusCode::NOT_FOUND).with_body_text_plain("Not Found\n")
    };

    resp.with_cors().send_to_client();

    Ok(())
}
