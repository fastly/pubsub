use fastly::{Error, Request};
use pubsub::{auth, config, routes, storage};
use std::env;

fn main() -> Result<(), Error> {
    let fastly_host = env::var("FASTLY_HOSTNAME").unwrap_or("localhost".to_string());
    let local = fastly_host == "localhost";
    let req = Request::from_client();

    let authorizor = auth::KVStoreAuthorizor::new("keys");
    let storage = storage::KVStoreStorage::new("messages");

    if local {
        let config_source = config::TestSource;

        routes::handle_request(&config_source, &authorizor, &storage, false, false, req)?;
    } else {
        let config_source = config::ConfigAndSecretStoreSource::new("config", "secrets");
        let fastly_authed = req.fastly_key_is_valid();

        routes::handle_request(
            &config_source,
            &authorizor,
            &storage,
            true,
            fastly_authed,
            req,
        )?;
    }

    Ok(())
}
