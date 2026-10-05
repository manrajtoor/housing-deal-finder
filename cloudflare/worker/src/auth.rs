//! Who may call what: bearer token for the crawler routes, and where reads
//! are served.

/// Host the Pages Function uses when it calls this Worker over the service
/// binding. Public traffic is routed by real hostnames, so a request can only
/// carry this host when it comes through the binding.
pub const INTERNAL_HOST: &str = "housedeals-api.internal";

/// Read routes answer on the internal host always, and on the public
/// workers.dev host only when `PUBLIC_READ_API` is "true" (local dev).
pub fn read_allowed(host: Option<&str>, public_read_api: bool) -> bool {
    public_read_api || host == Some(INTERNAL_HOST)
}

/// True when `header` is exactly `Bearer <expected>`. The comparison takes the
/// same time wherever the first difference is. An empty expected token never
/// matches.
pub fn bearer_ok(header: Option<&str>, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let Some(given) = header.and_then(|h| h.strip_prefix("Bearer ")) else {
        return false;
    };
    constant_time_eq(given.trim().as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_exact_token() {
        assert!(bearer_ok(Some("Bearer s3cret"), "s3cret"));
        assert!(!bearer_ok(Some("Bearer s3creT"), "s3cret"));
        assert!(!bearer_ok(Some("Bearer s3cret2"), "s3cret"));
        assert!(!bearer_ok(Some("s3cret"), "s3cret"));
        assert!(!bearer_ok(Some("Basic s3cret"), "s3cret"));
        assert!(!bearer_ok(None, "s3cret"));
        assert!(!bearer_ok(Some("Bearer "), ""), "no configured token means closed");
    }

    #[test]
    fn reads_only_through_the_binding_unless_public() {
        assert!(read_allowed(Some(INTERNAL_HOST), false));
        assert!(!read_allowed(Some("housedeals-api.me.workers.dev"), false));
        assert!(!read_allowed(None, false));
        assert!(read_allowed(Some("housedeals-api.me.workers.dev"), true));
    }
}
