//! Google Cloud access tokens for Vertex AI.
//!
//! Sources, in order: a service account key (`credentials_json` /
//! `credentials_file` in the provider config, or the file named by
//! `GOOGLE_APPLICATION_CREDENTIALS`), user credentials from
//! `gcloud auth application-default login` (same file, `authorized_user`),
//! then the GCE / GKE / Cloud Run metadata server. Tokens are cached until
//! five minutes before they expire.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use parking_lot::Mutex;
use serde_json::{json, Value};

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone)]
struct Cached {
    token: String,
    expires_at: u64,
}

#[derive(Default)]
pub struct TokenSource {
    cached: Mutex<Option<Cached>>,
}

/// Creates a signed RS256 JWT assertion for a service account.
pub fn service_account_assertion(key: &Value, now: u64) -> Result<String, String> {
    let email =
        key.get("client_email").and_then(Value::as_str).ok_or("service account key has no client_email")?;
    let pem =
        key.get("private_key").and_then(Value::as_str).ok_or("service account key has no private_key")?;
    let token_uri = key.get("token_uri").and_then(Value::as_str).unwrap_or(DEFAULT_TOKEN_URI);
    let der = pem_to_der(pem)?;
    let pair =
        ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("invalid private_key: {e}"))?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = json!({"iss": email, "scope": SCOPE, "aud": token_uri, "iat": now, "exp": now + 3600});
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signing_input = format!("{header}.{claims}");
    let mut sig = vec![0u8; pair.public().modulus_len()];
    pair.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signing_input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| "signing the token request failed".to_string())?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|_| "private_key is not valid PEM".to_string())
}

impl TokenSource {
    /// A bearer token for Vertex. `key` is the service account or
    /// authorized-user JSON when one was configured.
    pub async fn token(&self, http: &reqwest::Client, key: Option<&Value>) -> Result<String, String> {
        if let Some(c) = self.cached.lock().clone() {
            if c.expires_at > now_unix() + 300 {
                return Ok(c.token);
            }
        }
        let env_key;
        let key = match key {
            Some(k) => Some(k),
            None => match std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok().filter(|p| !p.is_empty()) {
                Some(path) => {
                    let raw = std::fs::read_to_string(&path).map_err(|e| format!("reading {path}: {e}"))?;
                    env_key =
                        serde_json::from_str::<Value>(&raw).map_err(|e| format!("parsing {path}: {e}"))?;
                    Some(&env_key)
                }
                None => None,
            },
        };
        let resp = match key {
            Some(k) if k.get("type").and_then(Value::as_str) == Some("authorized_user") => {
                let s = |f: &str| k.get(f).and_then(Value::as_str).unwrap_or_default().to_string();
                http.post(DEFAULT_TOKEN_URI)
                    .form(&[
                        ("grant_type", "refresh_token".to_string()),
                        ("client_id", s("client_id")),
                        ("client_secret", s("client_secret")),
                        ("refresh_token", s("refresh_token")),
                    ])
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
            }
            Some(k) => {
                let assertion = service_account_assertion(k, now_unix())?;
                let uri = k.get("token_uri").and_then(Value::as_str).unwrap_or(DEFAULT_TOKEN_URI);
                http.post(uri)
                    .form(&[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                        ("assertion", &assertion),
                    ])
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
            }
            None => {
                let url = std::env::var("GCE_METADATA_TOKEN_URL")
                    .unwrap_or_else(|_| METADATA_TOKEN_URL.to_string());
                http.get(url).header("metadata-flavor", "Google").timeout(Duration::from_secs(3)).send().await
            }
        }
        .map_err(|e| format!("could not get a Google access token: {e}"))?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| format!("invalid token response: {e}"))?;
        if !status.is_success() {
            let why =
                v.get("error_description").or_else(|| v.get("error")).and_then(Value::as_str).unwrap_or("");
            return Err(format!("Google token endpoint returned {status}: {why}"));
        }
        let token =
            v.get("access_token").and_then(Value::as_str).ok_or("token response had no access_token")?;
        let ttl = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
        *self.cached.lock() = Some(Cached { token: token.to_string(), expires_at: now_unix() + ttl });
        Ok(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_keys() {
        assert!(service_account_assertion(&json!({}), 0).is_err());
        let k = json!({"client_email": "a@b", "private_key": "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n"});
        assert!(service_account_assertion(&k, 0).unwrap_err().contains("invalid private_key"));
    }

    #[test]
    fn signs_a_verifiable_jwt() {
        let pem = include_str!("../tests/fixtures/test-service-account.pem");
        let key = json!({"client_email": "gw@p1.iam.gserviceaccount.com", "private_key": pem, "token_uri": "https://t/token"});
        let jwt = service_account_assertion(&key, 1000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://t/token");
        assert_eq!(claims["exp"], 4600);
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&pem_to_der(pem).unwrap()).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            pair.public().as_ref().to_vec(),
        );
        let sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        public.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }
}
