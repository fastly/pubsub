use fastly::{config_store, secret_store};
use std::str;

pub struct Config {
    pub sse_enabled: bool,
    pub http_publish_enabled: bool,
    pub mqtt_enabled: bool,
    pub admin_enabled: bool,
    pub publish_token: String,
    pub internal_key: Vec<u8>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sse_enabled: true,
            http_publish_enabled: true,
            mqtt_enabled: true,
            admin_enabled: true,
            publish_token: String::new(),
            internal_key: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    StoreError,
    InvalidValue,
}

impl From<config_store::LookupError> for ConfigError {
    fn from(_e: config_store::LookupError) -> Self {
        Self::StoreError
    }
}

fn str_to_bool(s: &str) -> Result<bool, ConfigError> {
    match s.parse() {
        Ok(b) => Ok(b),
        Err(_) => Err(ConfigError::InvalidValue),
    }
}

pub trait Source {
    fn config(&self) -> Result<Config, ConfigError>;
}

pub struct ConfigAndSecretStoreSource {
    config_store_name: String,
    secret_store_name: String,
}

impl ConfigAndSecretStoreSource {
    pub fn new(config_store_name: &str, secret_store_name: &str) -> Self {
        Self {
            config_store_name: config_store_name.to_string(),
            secret_store_name: secret_store_name.to_string(),
        }
    }
}

impl Source for ConfigAndSecretStoreSource {
    fn config(&self) -> Result<Config, ConfigError> {
        let config_store = match config_store::ConfigStore::try_open(&self.config_store_name) {
            Ok(store) => Some(store),
            Err(config_store::OpenError::ConfigStoreDoesNotExist) => None,
            Err(_) => return Err(ConfigError::StoreError),
        };

        let secret_store = match secret_store::SecretStore::open(&self.secret_store_name) {
            Ok(store) => Some(store),
            Err(secret_store::OpenError::SecretStoreDoesNotExist(_)) => None,
            Err(_) => return Err(ConfigError::StoreError),
        };

        let mut config = Config::default();

        if let Some(store) = &config_store {
            if let Some(v) = store.try_get("sse")? {
                config.sse_enabled = str_to_bool(&v)?;
            }

            if let Some(v) = store.try_get("http-publish")? {
                config.http_publish_enabled = str_to_bool(&v)?;
            }

            if let Some(v) = store.try_get("admin")? {
                config.admin_enabled = str_to_bool(&v)?;
            }
        }

        if let Some(store) = &secret_store {
            match store.try_get("publish-token") {
                Ok(Some(v)) => {
                    let v = match str::from_utf8(&v.plaintext()) {
                        Ok(s) => s.to_string(),
                        Err(_) => return Err(ConfigError::InvalidValue),
                    };

                    config.publish_token = v;
                }
                Ok(None) => {}
                Err(_) => return Err(ConfigError::StoreError),
            }

            match store.try_get("internal-key") {
                Ok(Some(v)) => config.internal_key = v.plaintext().to_vec(),
                Ok(None) => {}
                Err(_) => return Err(ConfigError::StoreError),
            }
        }

        Ok(config)
    }
}

pub struct TestSource;

impl Source for TestSource {
    fn config(&self) -> Result<Config, ConfigError> {
        Ok(Config::default())
    }
}
