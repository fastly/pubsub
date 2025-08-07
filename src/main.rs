use fastly::{Error, Request};
use pubsub::{auth, config, routes, storage};
use std::env;

fn main() -> Result<(), Error> {
    let fastly_host = env::var("FASTLY_HOSTNAME").unwrap_or("localhost".to_string());
    let local = fastly_host == "localhost";
    let req = Request::from_client();

    let app_token_authorizor = Box::new(auth::KVStoreAppTokenAuthorizor::new("keys"));
    let storage = storage::KVStoreStorage::new("messages");

    let (config_source, auth) = if local {
        let config_source: Box<dyn config::Source> = Box::new(config::TestSource);

        let auth = auth::Authorization {
            grip: Box::new(auth::TestGripAuthorizor),
            fastly: false,
            app_token: app_token_authorizor,
        };

        (config_source, auth)
    } else {
        let config_source: Box<dyn config::Source> =
            Box::new(config::ConfigAndSecretStoreSource::new("config", "secrets"));

        let auth = auth::Authorization {
            grip: Box::new(auth::FanoutGripAuthorizor),
            fastly: req.fastly_key_is_valid(),
            app_token: app_token_authorizor,
        };

        (config_source, auth)
    };

    routes::handle_request(&*config_source, &auth, &storage, req)?;

    Ok(())
}
