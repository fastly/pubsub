use jwt_simple::prelude::*;
use std::env;
use thiserror::Error;

const FASTLY_PUBLIC_KEY: &str = concat!(
    "-----BEGIN PUBLIC KEY-----\n",
    "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAECKo5A1ebyFcnmVV8SE5On+8G81Jy\n",
    "BjSvcrx4VLetWCjuDAmppTo3xM/zz763COTCgHfp/6lPdCyYjjqc+GM7sw==\n",
    "-----END PUBLIC KEY-----\n"
);

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("token verification failed: {0}")]
    Verify(#[from] jwt_simple::Error),

    #[error("token has no issuer")]
    NoIssuer,

    #[error("token was issued for a different service ID: {0}")]
    ServiceMismatch(String),
}

pub fn validate_grip_sig(sig: &str) -> Result<(), ValidationError> {
    let key = ES256PublicKey::from_pem(FASTLY_PUBLIC_KEY).expect("public key should be parsable");

    let service_id = env::var("FASTLY_SERVICE_ID").expect("FASTLY_SERVICE_ID should be set");

    let claims = key.verify_token::<NoCustomClaims>(sig, None)?;

    let Some(issuer) = claims.issuer else {
        return Err(ValidationError::NoIssuer);
    };

    if issuer != format!("fastly:{service_id}") {
        return Err(ValidationError::ServiceMismatch(issuer));
    }

    Ok(())
}

#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct ControlMessage {
    #[serde(rename(serialize = "type"))]
    pub ctype: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fastly_key() {
        ES256PublicKey::from_pem(FASTLY_PUBLIC_KEY).unwrap();
    }
}
