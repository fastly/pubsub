use fastly::{Error, Request};
use pubsub::{auth, config, routes, storage};
use std::env;

fn main() -> Result<(), Error> {
    let fastly_host = env::var("FASTLY_HOSTNAME").unwrap_or("localhost".to_string());
    let local = fastly_host == "localhost";
    let req = Request::from_client();

    let storage = storage::KVStoreStorage::new("messages");

    if local {
        let config_source = config::TestSource;
        let authorizor = auth::TestAuthorizor;

        routes::handle_request(&config_source, &authorizor, &storage, req)?;
    } else {
        let config_source = config::ConfigAndSecretStoreSource::new("config", "secrets");
        let authorizor = auth::KVStoreAuthorizor::new("keys");

        routes::handle_request(&config_source, &authorizor, &storage, req)?;
    }

    Ok(())
}
