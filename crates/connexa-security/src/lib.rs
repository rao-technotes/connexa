//! Security primitives: room codes, secret tokens, rate limiting and TURN credentials.
//!
//! A room code is an *identifier*, never an encryption key. WebRTC's DTLS-SRTP
//! provides media encryption independently of it.

use std::hash::Hash;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use connexa_core::{MAX_DISPLAY_NAME_CHARS, ROOM_CODE_LEN};
use dashmap::DashMap;
use hmac::{Hmac, Mac};

/// Fill `buf` from the operating system CSPRNG.
fn os_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("operating system random source is unavailable");
}

/// Uniform random integer in `0..bound` from the OS CSPRNG (rejection sampling, no modulo bias).
fn random_below(bound: u32) -> u32 {
    let zone = u32::MAX - (u32::MAX % bound);
    loop {
        let mut bytes = [0u8; 4];
        os_random(&mut bytes);
        let n = u32::from_le_bytes(bytes);
        if n < zone {
            return n % bound;
        }
    }
}

/// Generate a 9-digit room code. The first digit is never zero so codes
/// survive being typed into numeric fields.
pub fn generate_room_code() -> String {
    const LOW: u32 = 100_000_000;
    const HIGH: u32 = 999_999_999;
    (LOW + random_below(HIGH - LOW + 1)).to_string()
}

pub fn validate_room_id(room_id: &str) -> bool {
    room_id.len() == ROOM_CODE_LEN && room_id.bytes().all(|c| c.is_ascii_digit())
}

/// Accept user input such as `847 291 653` or `847-291-653`.
pub fn normalize_room_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    validate_room_id(&code).then_some(code)
}

/// `847291653` -> `847 291 653`.
pub fn format_room_code(code: &str) -> String {
    code.as_bytes()
        .chunks(3)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `847291653` -> `847***653`. Room codes grant access, so logs only see this form.
pub fn redact_room_code(code: &str) -> String {
    if code.len() == ROOM_CODE_LEN {
        format!("{}***{}", &code[..3], &code[6..])
    } else {
        "<invalid>".to_string()
    }
}

/// 128-bit random token, hex encoded.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    os_random(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Short random identifier for participants (64 bits, hex).
pub fn generate_participant_id() -> String {
    let mut bytes = [0u8; 8];
    os_random(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare secrets without leaking the position of the first mismatch.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Trim, strip control characters and cap length. Falls back to `fallback` when empty.
pub fn sanitize_display_name(input: Option<&str>, fallback: &str) -> String {
    let cleaned: String = input
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DISPLAY_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

/// Token-bucket rate limiter keyed by e.g. client IP.
pub struct RateLimiter<K: Eq + Hash> {
    buckets: DashMap<K, Bucket>,
    capacity: f64,
    refill_per_sec: f64,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    /// Allows bursts of `capacity`, refilling to it over `per`.
    pub fn new(capacity: u32, per: Duration) -> Self {
        Self {
            buckets: DashMap::new(),
            capacity: capacity as f64,
            refill_per_sec: capacity as f64 / per.as_secs_f64(),
        }
    }

    pub fn check(&self, key: &K) -> bool {
        self.check_at(key, Instant::now())
    }

    pub fn check_at(&self, key: &K, now: Instant) -> bool {
        let mut bucket = self.buckets.entry(key.clone()).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Forget keys whose bucket has fully refilled, bounding memory use.
    pub fn sweep(&self, now: Instant) {
        let full_after = self.capacity / self.refill_per_sec;
        self.buckets
            .retain(|_, b| now.saturating_duration_since(b.last).as_secs_f64() < full_after);
    }

    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// Time-limited TURN credentials using the coturn "TURN REST API" scheme
/// (`use-auth-secret` / `static-auth-secret`): the TURN server shares `secret`
/// with us and verifies `credential == base64(HMAC-SHA1(secret, username))`.
pub fn turn_rest_credentials(secret: &str, user: &str, ttl: Duration) -> (String, String) {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + ttl.as_secs();
    let username = format!("{expiry}:{user}");
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(username.as_bytes());
    let credential = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    (username, credential)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn room_codes_are_nine_digits() {
        for _ in 0..1000 {
            let code = generate_room_code();
            assert!(validate_room_id(&code), "{code}");
            assert_ne!(code.as_bytes()[0], b'0');
        }
    }

    #[test]
    fn room_codes_do_not_repeat_trivially() {
        let codes: HashSet<_> = (0..1000).map(|_| generate_room_code()).collect();
        assert!(codes.len() > 990);
    }

    #[test]
    fn validates_room_ids() {
        assert!(validate_room_id("847291653"));
        assert!(!validate_room_id("84729165"));
        assert!(!validate_room_id("8472916531"));
        assert!(!validate_room_id("84729165a"));
        assert!(!validate_room_id("８４７２９１６５３")); // full-width digits
    }

    #[test]
    fn normalizes_and_formats() {
        assert_eq!(
            normalize_room_code(" 847 291-653 ").as_deref(),
            Some("847291653")
        );
        assert_eq!(normalize_room_code("847 291"), None);
        assert_eq!(format_room_code("847291653"), "847 291 653");
        assert_eq!(redact_room_code("847291653"), "847***653");
    }

    #[test]
    fn tokens_are_random_hex() {
        let a = generate_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, generate_token());
        assert!(constant_time_eq(&a, &a.clone()));
        assert!(!constant_time_eq(&a, &generate_token()));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn sanitizes_display_names() {
        assert_eq!(
            sanitize_display_name(Some("  Ann\u{0007}  "), "Guest"),
            "Ann"
        );
        assert_eq!(sanitize_display_name(Some("   "), "Guest"), "Guest");
        assert_eq!(sanitize_display_name(None, "Guest"), "Guest");
        assert_eq!(
            sanitize_display_name(Some(&"x".repeat(100)), "Guest").len(),
            MAX_DISPLAY_NAME_CHARS
        );
    }

    #[test]
    fn rate_limiter_blocks_then_refills() {
        let limiter = RateLimiter::new(3, Duration::from_secs(3));
        let t0 = Instant::now();
        assert!(limiter.check_at(&"ip", t0));
        assert!(limiter.check_at(&"ip", t0));
        assert!(limiter.check_at(&"ip", t0));
        assert!(!limiter.check_at(&"ip", t0));
        assert!(limiter.check_at(&"other", t0));
        assert!(limiter.check_at(&"ip", t0 + Duration::from_millis(1100)));
        limiter.sweep(t0 + Duration::from_secs(10));
        assert!(limiter.is_empty());
    }

    #[test]
    fn turn_credentials_match_coturn_scheme() {
        let (user, cred) = turn_rest_credentials("secret", "p1", Duration::from_secs(60));
        let (expiry, id) = user.split_once(':').unwrap();
        assert!(expiry.parse::<u64>().is_ok());
        assert_eq!(id, "p1");
        assert_eq!(cred.len(), 28); // base64 of a 20-byte SHA-1 MAC
    }
}
