use crate::grip;
use fastly::kv_store;
use jwt_simple::prelude::*;
use std::borrow::Borrow;

pub trait GripAuthorizor {
    fn validate_sig(&self, sig: &str) -> Result<(), grip::ValidationError>;
}

pub struct FanoutGripAuthorizor;

impl GripAuthorizor for FanoutGripAuthorizor {
    fn validate_sig(&self, sig: &str) -> Result<(), grip::ValidationError> {
        grip::validate_grip_sig(sig)
    }
}

pub struct TestGripAuthorizor;

impl GripAuthorizor for TestGripAuthorizor {
    fn validate_sig(&self, _sig: &str) -> Result<(), grip::ValidationError> {
        Ok(())
    }
}

fn slice_contains<T, Q>(s: &[T], value: &Q) -> bool
where
    T: Borrow<Q>,
    Q: Eq + ?Sized,
{
    s.iter().any(|i| i.borrow() == value)
}

pub struct Capabilities {
    admin: bool,
    read: Vec<String>,
    write: Vec<String>,
}

impl Capabilities {
    pub fn new_admin() -> Self {
        Self {
            admin: true,
            read: Vec::new(),
            write: Vec::new(),
        }
    }

    pub fn can_subscribe(&self, topic: &str) -> bool {
        if self.admin {
            return true;
        }

        slice_contains(&self.read, topic)
    }

    pub fn can_publish(&self, topic: &str) -> bool {
        if self.admin {
            return true;
        }

        slice_contains(&self.write, topic)
    }
}

#[derive(Debug)]
pub enum TokenError {
    Invalid,
    NoKeyId,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct CustomClaims {
    #[serde(default)]
    x_fastly_read: Vec<String>,

    #[serde(default)]
    x_fastly_write: Vec<String>,
}

fn validate_token(token: &str, key: &[u8]) -> Result<Capabilities, TokenError> {
    let key = HS256Key::from_bytes(key);

    let options = VerificationOptions::default();

    let claims = match key.verify_token::<CustomClaims>(token, Some(options)) {
        Ok(claims) => claims,
        Err(_) => return Err(TokenError::Invalid),
    };

    let caps = Capabilities {
        admin: false,
        read: claims.custom.x_fastly_read,
        write: claims.custom.x_fastly_write,
    };

    Ok(caps)
}

#[derive(Debug)]
pub enum AuthorizationError {
    Token(TokenError),
    StoreNotFound,
    StoreError,
    KeyNotFound,
}

impl From<TokenError> for AuthorizationError {
    fn from(e: TokenError) -> Self {
        Self::Token(e)
    }
}

pub trait AppTokenAuthorizor {
    fn validate_token(&self, token: &str) -> Result<Capabilities, AuthorizationError>;
}

pub struct KVStoreAppTokenAuthorizor {
    store_name: String,
}

impl KVStoreAppTokenAuthorizor {
    pub fn new(store_name: &str) -> Self {
        Self {
            store_name: store_name.to_string(),
        }
    }
}

impl AppTokenAuthorizor for KVStoreAppTokenAuthorizor {
    fn validate_token(&self, token: &str) -> Result<Capabilities, AuthorizationError> {
        let Ok(metadata) = Token::decode_metadata(token) else {
            return Err(AuthorizationError::Token(TokenError::Invalid));
        };

        let Some(key_id) = metadata.key_id() else {
            return Err(AuthorizationError::Token(TokenError::NoKeyId));
        };

        let store = match kv_store::KVStore::open(&self.store_name) {
            Ok(Some(store)) => store,
            Ok(None) => return Err(AuthorizationError::StoreNotFound),
            Err(_) => return Err(AuthorizationError::StoreError),
        };

        let v = match store.lookup(key_id) {
            Ok(mut lookup) => lookup.take_body_bytes(),
            Err(kv_store::KVStoreError::ItemNotFound) => {
                return Err(AuthorizationError::KeyNotFound)
            }
            Err(_) => return Err(AuthorizationError::StoreError),
        };

        Ok(validate_token(token, &v)?)
    }
}

pub struct TestAppTokenAuthorizor;

impl AppTokenAuthorizor for TestAppTokenAuthorizor {
    fn validate_token(&self, token: &str) -> Result<Capabilities, AuthorizationError> {
        Ok(validate_token(token, b"notasecret")?)
    }
}

pub struct Authorization {
    pub grip: Box<dyn GripAuthorizor>,
    pub fastly: bool,
    pub app_token: Box<dyn AppTokenAuthorizor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_auth() {
        let claims = Claims::with_custom_claims(
            CustomClaims {
                x_fastly_read: vec!["readable".to_string()],
                x_fastly_write: vec!["writable".to_string()],
            },
            Duration::from_secs(60),
        );

        let key = HS256Key::from_bytes(b"notasecret");
        let token = key.authenticate(claims).unwrap();

        let caps = TestAppTokenAuthorizor.validate_token(&token).unwrap();
        assert!(caps.can_subscribe("readable"));
        assert!(!caps.can_subscribe("foo"));
        assert!(caps.can_publish("writable"));
        assert!(!caps.can_subscribe("foo"));
    }
}
