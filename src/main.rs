use fastly::{Error, Request};
use pubsub::{auth, config, routes};
use std::env;

fn main() -> Result<(), Error> {
    let fastly_host = env::var("FASTLY_HOSTNAME").unwrap_or("localhost".to_string());
    let local = fastly_host == "localhost";
    let req = Request::from_client();

    if local {
        let config_source = config::TestSource;
        let authorizor = auth::TestAuthorizor;

        routes::handle_request(&config_source, &authorizor, req)?;
    } else {
        let config_source = config::ConfigAndSecretStoreSource::new("config", "secrets");
        let authorizor = auth::KVStoreAuthorizor::new("keys");

        routes::handle_request(&config_source, &authorizor, req)?;
    }

    Ok(())
}
