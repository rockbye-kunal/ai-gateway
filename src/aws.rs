//! AWS support for Bedrock: Signature Version 4, the standard credential
//! chain, and the `application/vnd.amazon.eventstream` framing used by
//! streaming responses.
//!
//! Credentials are looked up in this order and cached until shortly before
//! they expire:
//! 1. `aws_access_key_id` / `aws_secret_access_key` / `aws_session_token` in
//!    the provider config;
//! 2. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`;
//! 3. web identity (EKS IRSA): `AWS_ROLE_ARN` + `AWS_WEB_IDENTITY_TOKEN_FILE`;
//! 4. ECS task role: `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` / `_FULL_URI`;
//! 5. EC2 instance role through IMDSv2.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Unix seconds; None for long-lived keys.
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials").field("access_key_id", &self.access_key_id).finish_non_exhaustive()
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3986 encoding as SigV4 wants it (`/` kept when `keep_slash`).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn amz_date(unix: u64) -> (String, String) {
    let days = (unix / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let s = unix % 86_400;
    let date = format!("{y:04}{m:02}{d:02}");
    (format!("{date}T{:02}{:02}{:02}Z", s / 3600, (s % 3600) / 60, s % 60), date)
}

/// Signs a request in place: adds `x-amz-date`, `x-amz-content-sha256`,
/// `x-amz-security-token` (for temporary credentials) and `authorization`.
pub fn sign(req: &mut reqwest::Request, creds: &AwsCredentials, region: &str, service: &str, now: u64) {
    let (amz, date) = amz_date(now);
    let body = req.body().and_then(|b| b.as_bytes()).unwrap_or_default().to_vec();
    let payload_hash = sha256_hex(&body);
    let url = req.url().clone();
    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let h = req.headers_mut();
    h.insert("x-amz-date", amz.parse().expect("valid date header"));
    h.insert("x-amz-content-sha256", payload_hash.parse().expect("valid hash header"));
    if let Some(t) = &creds.session_token {
        if let Ok(mut v) = reqwest::header::HeaderValue::from_str(t) {
            v.set_sensitive(true);
            h.insert("x-amz-security-token", v);
        }
    }
    // reqwest adds Host itself; it is only added here for the signature.
    let mut signed = req.headers().clone();
    signed.insert("host", host.parse().expect("valid host header"));
    let auth = authorization(
        req.method().as_str(),
        &url,
        &signed,
        &payload_hash,
        creds,
        region,
        service,
        &amz,
        &date,
    );
    let mut v = reqwest::header::HeaderValue::from_str(&auth).expect("ascii authorization");
    v.set_sensitive(true);
    req.headers_mut().insert("authorization", v);
}

/// The `authorization` header value for a request. `headers` must include
/// `host` and `x-amz-date`; `amz` is that date and `date` its first 8 chars.
/// Public so mock servers can check signatures.
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    method: &str,
    url: &reqwest::Url,
    headers: &reqwest::header::HeaderMap,
    payload_hash: &str,
    creds: &AwsCredentials,
    region: &str,
    service: &str,
    amz: &str,
    date: &str,
) -> String {
    // Canonical URI: the path as sent, URI-encoded once more (SigV4 double
    // encoding for every service but S3). A Bedrock model id `a:b` is sent
    // as `a%3Ab` and signed as `a%253Ab`.
    let path = url.path();
    let canonical_uri = if path.is_empty() { "/".to_string() } else { uri_encode(path, true) };
    let mut query: Vec<(String, String)> =
        url.query_pairs().map(|(k, v)| (uri_encode(&k, false), uri_encode(&v, false))).collect();
    query.sort();
    let canonical_query = query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    let mut signed: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            k == "host" || k == "content-type" || k.starts_with("x-amz-")
        })
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().trim().to_string()))
        .collect();
    signed.sort();
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = signed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let canonical = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
    let k_date = hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Caches credentials from the chain until five minutes before expiry.
#[derive(Default)]
pub struct CredentialCache {
    cached: Mutex<Option<AwsCredentials>>,
}

impl CredentialCache {
    pub async fn get(
        &self,
        http: &reqwest::Client,
        configured: Option<AwsCredentials>,
    ) -> Result<AwsCredentials, String> {
        if let Some(c) = configured {
            return Ok(c);
        }
        if let Some(c) = self.cached.lock().clone() {
            if c.expires_at.is_none_or(|e| e > now_unix() + 300) {
                return Ok(c);
            }
        }
        let c = chain(http).await?;
        *self.cached.lock() = Some(c.clone());
        Ok(c)
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

async fn chain(http: &reqwest::Client) -> Result<AwsCredentials, String> {
    if let (Some(id), Some(secret)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
        return Ok(AwsCredentials {
            access_key_id: id,
            secret_access_key: secret,
            session_token: env("AWS_SESSION_TOKEN"),
            expires_at: None,
        });
    }
    if let (Some(role), Some(file)) = (env("AWS_ROLE_ARN"), env("AWS_WEB_IDENTITY_TOKEN_FILE")) {
        return web_identity(http, &role, &file).await;
    }
    if let Some(uri) = env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        return container(http, &format!("http://169.254.170.2{uri}")).await;
    }
    if let Some(uri) = env("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        return container(http, &uri).await;
    }
    imds(http).await.map_err(|e| {
        format!(
            "no AWS credentials found (config, environment, web identity, container or instance role): {e}"
        )
    })
}

fn parse_iso8601(s: &str) -> Option<u64> {
    // 2026-11-06T12:00:00Z
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, u32, i64) =
        (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let t = time.trim_end_matches('Z');
    let t = t.split('.').next()?;
    let mut tp = t.split(':');
    let (hh, mm, ss): (u64, u64, u64) =
        (tp.next()?.parse().ok()?, tp.next()?.parse().ok()?, tp.next()?.parse().ok()?);
    let (y2, m2) = if m <= 2 { (y - 1, i64::from(m) + 9) } else { (y, i64::from(m) - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days as u64 * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn from_json(v: &Value) -> Result<AwsCredentials, String> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(AwsCredentials {
        access_key_id: s("AccessKeyId").ok_or("credentials response had no AccessKeyId")?,
        secret_access_key: s("SecretAccessKey").ok_or("credentials response had no SecretAccessKey")?,
        session_token: s("Token").or_else(|| s("SessionToken")),
        expires_at: s("Expiration").as_deref().and_then(parse_iso8601),
    })
}

async fn container(http: &reqwest::Client, url: &str) -> Result<AwsCredentials, String> {
    let mut rb = http.get(url).timeout(Duration::from_secs(5));
    if let Some(t) = env("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
        rb = rb.header("authorization", t);
    } else if let Some(f) = env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        if let Ok(t) = std::fs::read_to_string(f) {
            rb = rb.header("authorization", t.trim().to_string());
        }
    }
    let v: Value = rb.send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    from_json(&v)
}

async fn imds(http: &reqwest::Client) -> Result<AwsCredentials, String> {
    let base = "http://169.254.169.254/latest";
    let token = http
        .put(format!("{base}/api/token"))
        .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let get = |path: String| {
        http.get(format!("{base}/meta-data/iam/security-credentials/{path}"))
            .header("x-aws-ec2-metadata-token", token.clone())
            .timeout(Duration::from_secs(2))
            .send()
    };
    let role =
        get(String::new()).await.map_err(|e| e.to_string())?.text().await.map_err(|e| e.to_string())?;
    let role = role.lines().next().ok_or("instance has no IAM role")?.trim().to_string();
    let v: Value = get(role).await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    from_json(&v)
}

fn xml_tag(body: &str, tag: &str) -> Option<String> {
    let start = body.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = body[start..].find(&format!("</{tag}>"))? + start;
    Some(body[start..end].to_string())
}

async fn web_identity(
    http: &reqwest::Client,
    role: &str,
    token_file: &str,
) -> Result<AwsCredentials, String> {
    let token = std::fs::read_to_string(token_file).map_err(|e| format!("reading {token_file}: {e}"))?;
    let session = env("AWS_ROLE_SESSION_NAME").unwrap_or_else(|| "ai-gateway".into());
    let endpoint = match env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")) {
        Some(r) => format!("https://sts.{r}.amazonaws.com/"),
        None => "https://sts.amazonaws.com/".into(),
    };
    let body = http
        .post(endpoint)
        .form(&[
            ("Action", "AssumeRoleWithWebIdentity"),
            ("Version", "2011-06-15"),
            ("RoleArn", role),
            ("RoleSessionName", session.as_str()),
            ("WebIdentityToken", token.trim()),
        ])
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    Ok(AwsCredentials {
        access_key_id: xml_tag(&body, "AccessKeyId").ok_or("STS response had no credentials")?,
        secret_access_key: xml_tag(&body, "SecretAccessKey").ok_or("STS response had no credentials")?,
        session_token: xml_tag(&body, "SessionToken"),
        expires_at: xml_tag(&body, "Expiration").as_deref().and_then(parse_iso8601),
    })
}

// ---------- event stream ----------

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c = CRC_TABLE[((c ^ u32::from(*b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// Decodes `application/vnd.amazon.eventstream` frames. Each event comes out
/// as a JSON string with `"type"` set from the `:event-type` header (or
/// `:exception-type`, as `"exception:<name>"`), merged with the payload.
#[derive(Default)]
pub struct EventStreamParser {
    buf: Vec<u8>,
}

impl EventStreamParser {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, &'static str> {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > 16 * 1024 * 1024 {
            return Err("upstream event too large");
        }
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 12 {
                break;
            }
            let total = u32::from_be_bytes(self.buf[0..4].try_into().expect("4 bytes")) as usize;
            let headers_len = u32::from_be_bytes(self.buf[4..8].try_into().expect("4 bytes")) as usize;
            if total < 16 || headers_len > total - 16 {
                return Err("malformed upstream event");
            }
            if self.buf.len() < total {
                break;
            }
            let frame: Vec<u8> = self.buf.drain(..total).collect();
            let prelude_crc = u32::from_be_bytes(frame[8..12].try_into().expect("4 bytes"));
            let msg_crc = u32::from_be_bytes(frame[total - 4..].try_into().expect("4 bytes"));
            if crc32(&frame[..8]) != prelude_crc || crc32(&frame[..total - 4]) != msg_crc {
                return Err("corrupted upstream event");
            }
            let headers = parse_headers(&frame[12..12 + headers_len]);
            let payload = &frame[12 + headers_len..total - 4];
            let mut v: Value = serde_json::from_slice(payload).unwrap_or(Value::Object(Default::default()));
            let kind = match (
                headers.get(":message-type").map(String::as_str),
                headers.get(":event-type"),
                headers.get(":exception-type"),
            ) {
                (Some("exception"), _, Some(e)) | (Some("error"), _, Some(e)) => format!("exception:{e}"),
                (Some("error"), _, _) => {
                    format!("exception:{}", headers.get(":error-code").cloned().unwrap_or_default())
                }
                (_, Some(e), _) => e.clone(),
                _ => String::new(),
            };
            if let Some(o) = v.as_object_mut() {
                o.insert("type".into(), Value::String(kind));
            }
            out.push(v.to_string());
        }
        Ok(out)
    }
}

fn parse_headers(mut b: &[u8]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    while !b.is_empty() {
        let n = b[0] as usize;
        if b.len() < 2 + n {
            break;
        }
        let name = String::from_utf8_lossy(&b[1..1 + n]).into_owned();
        let ty = b[1 + n];
        b = &b[2 + n..];
        let value = match ty {
            7 => {
                if b.len() < 2 {
                    break;
                }
                let len = u16::from_be_bytes([b[0], b[1]]) as usize;
                if b.len() < 2 + len {
                    break;
                }
                let s = String::from_utf8_lossy(&b[2..2 + len]).into_owned();
                b = &b[2 + len..];
                s
            }
            // Other value types are not used by Bedrock; skip known sizes.
            0 | 1 => String::new(),
            2 => {
                b = b.get(1..).unwrap_or_default();
                String::new()
            }
            3 => {
                b = b.get(2..).unwrap_or_default();
                String::new()
            }
            4 => {
                b = b.get(4..).unwrap_or_default();
                String::new()
            }
            5 | 8 => {
                b = b.get(8..).unwrap_or_default();
                String::new()
            }
            9 => {
                b = b.get(16..).unwrap_or_default();
                String::new()
            }
            6 => {
                if b.len() < 2 {
                    break;
                }
                let len = u16::from_be_bytes([b[0], b[1]]) as usize;
                b = b.get(2 + len..).unwrap_or_default();
                String::new()
            }
            _ => break,
        };
        out.insert(name, value);
    }
    out
}

/// Encodes one event (used by tests and mock servers).
pub fn encode_event(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (k, v) in headers {
        h.push(k.len() as u8);
        h.extend_from_slice(k.as_bytes());
        h.push(7);
        h.extend_from_slice(&(v.len() as u16).to_be_bytes());
        h.extend_from_slice(v.as_bytes());
    }
    let total = 12 + h.len() + payload.len() + 4;
    let mut f = Vec::with_capacity(total);
    f.extend_from_slice(&(total as u32).to_be_bytes());
    f.extend_from_slice(&(h.len() as u32).to_be_bytes());
    let pc = crc32(&f);
    f.extend_from_slice(&pc.to_be_bytes());
    f.extend_from_slice(&h);
    f.extend_from_slice(payload);
    let mc = crc32(&f);
    f.extend_from_slice(&mc.to_be_bytes());
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn sigv4_get_vanilla() {
        // AWS SigV4 test suite: get-vanilla.
        let creds = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            expires_at: None,
        };
        let url = reqwest::Url::parse("https://example.amazonaws.com/").unwrap();
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("host", "example.amazonaws.com".parse().unwrap());
        h.insert("x-amz-date", "20150830T123600Z".parse().unwrap());
        let auth = authorization(
            "GET",
            &url,
            &h,
            &sha256_hex(b""),
            &creds,
            "us-east-1",
            "service",
            "20150830T123600Z",
            "20150830",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn dates_and_encoding() {
        assert_eq!(amz_date(1_440_938_160), ("20150830T123600Z".to_string(), "20150830".to_string()));
        assert_eq!(uri_encode("anthropic.claude-v2:1", false), "anthropic.claude-v2%3A1");
        assert_eq!(uri_encode("/model/a%3Ab/converse", true), "/model/a%253Ab/converse");
        assert_eq!(parse_iso8601("2015-08-30T12:36:00Z"), Some(1_440_938_160));
        assert_eq!(xml_tag("<a><AccessKeyId>X</AccessKeyId></a>", "AccessKeyId").as_deref(), Some("X"));
    }

    #[test]
    fn event_stream_round_trip() {
        let mut bytes = encode_event(
            &[(":event-type", "contentBlockDelta"), (":message-type", "event")],
            br#"{"delta":{"text":"Hi"}}"#,
        );
        bytes.extend(encode_event(
            &[(":event-type", "metadata"), (":message-type", "event")],
            br#"{"usage":{"inputTokens":3}}"#,
        ));
        let mut p = EventStreamParser::default();
        // Split across chunks.
        let mut events = p.push(&bytes[..10]).unwrap();
        events.extend(p.push(&bytes[10..]).unwrap());
        assert_eq!(events.len(), 2);
        let first: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(first["type"], "contentBlockDelta");
        assert_eq!(first["delta"]["text"], "Hi");
        let mut bad = encode_event(&[(":event-type", "x")], b"{}");
        let n = bad.len();
        bad[n - 6] ^= 1;
        assert!(EventStreamParser::default().push(&bad).is_err());
        let exc = encode_event(
            &[(":message-type", "exception"), (":exception-type", "throttlingException")],
            br#"{"message":"slow down"}"#,
        );
        let v: Value = serde_json::from_str(&EventStreamParser::default().push(&exc).unwrap()[0]).unwrap();
        assert_eq!(v["type"], "exception:throttlingException");
    }
}
