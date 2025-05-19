use fastly::kv_store;
use jwt_simple::prelude::*;
use std::borrow::Borrow;

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

pub fn create_token(
    readable: Vec<String>,
    writable: Vec<String>,
    key_id: &str,
    key: &[u8],
) -> String {
    let key = HS256Key::from_bytes(key).with_key_id(key_id);

    let claims = Claims::with_custom_claims(
        CustomClaims {
            x_fastly_read: readable,
            x_fastly_write: writable,
        },
        Duration::from_hours(1),
    );

    key.authenticate(claims)
        .expect("token creation should always succeed")
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

pub trait Authorizor {
    fn validate_token(
        &self,
        token: &str,
        internal_key: Option<&[u8]>,
    ) -> Result<Capabilities, AuthorizationError>;
}

pub struct KVStoreAuthorizor {
    store_name: String,
}

impl KVStoreAuthorizor {
    pub fn new(store_name: &str) -> Self {
        Self {
            store_name: store_name.to_string(),
        }
    }
}

impl Authorizor for KVStoreAuthorizor {
    fn validate_token(
        &self,
        token: &str,
        internal_key: Option<&[u8]>,
    ) -> Result<Capabilities, AuthorizationError> {
        let Ok(metadata) = Token::decode_metadata(token) else {
            return Err(AuthorizationError::Token(TokenError::Invalid));
        };

        let Some(key_id) = metadata.key_id() else {
            return Err(AuthorizationError::Token(TokenError::NoKeyId));
        };

        let key = if key_id == "internal" {
            let Some(internal_key) = internal_key else {
                return Err(AuthorizationError::KeyNotFound);
            };

            internal_key.to_vec()
        } else {
            let store = match kv_store::KVStore::open(&self.store_name) {
                Ok(Some(store)) => store,
                Ok(None) => return Err(AuthorizationError::StoreNotFound),
                Err(_) => return Err(AuthorizationError::StoreError),
            };

            match store.lookup(key_id) {
                Ok(mut lookup) => lookup.take_body_bytes(),
                Err(kv_store::KVStoreError::ItemNotFound) => {
                    return Err(AuthorizationError::KeyNotFound)
                }
                Err(_) => return Err(AuthorizationError::StoreError),
            }
        };

        Ok(validate_token(token, &key)?)
    }
}

pub struct TestAuthorizor;

impl Authorizor for TestAuthorizor {
    fn validate_token(
        &self,
        token: &str,
        _internal_key: Option<&[u8]>,
    ) -> Result<Capabilities, AuthorizationError> {
        Ok(validate_token(token, b"notasecret")?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth() {
        let claims = Claims::with_custom_claims(
            CustomClaims {
                x_fastly_read: vec!["readable".to_string()],
                x_fastly_write: vec!["writable".to_string()],
            },
            Duration::from_secs(60),
        );

        let key = HS256Key::from_bytes(b"notasecret");
        let token = key.authenticate(claims).unwrap();

        let caps = TestAuthorizor.validate_token(&token, None).unwrap();
        assert!(caps.can_subscribe("readable"));
        assert!(!caps.can_subscribe("foo"));
        assert!(caps.can_publish("writable"));
        assert!(!caps.can_subscribe("foo"));
    }
}
