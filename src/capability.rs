//! Scoped, expiring authorization for one command on one incident.
//!
//! Notification buttons, links and reply messages carry one of these instead of a
//! reusable credential: never the dashboard's control token, never the ntfy access token.
//! A capability names its command and incident, expires, and dies with the incident.
use sha2::{Digest, Sha256};

/// Longest lifetime a capability may be issued with or accepted for.
pub const MAX_LIFETIME_S: f64 = 86400.0;

/// Scope for fetching the latest analyzed frame (webhook `image_url`); not a printer command.
pub const READ_FRAME: &str = "read-frame";

/// Why a presented capability was or was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Valid,
    /// Genuine, but past its expiry. Safe to tell the holder so.
    Expired,
    /// Not signed by this installation for this command and subject.
    Invalid,
}

#[derive(Clone)]
pub struct Signer(pub [u8; 32]);

impl Default for Signer {
    fn default() -> Self {
        Self(rand::random())
    }
}

impl Signer {
    // HMAC-SHA256, with a 32-byte random key padded to SHA256's 64-byte block.
    pub fn sign(&self, command: &str, incident: &str, expires: i64) -> String {
        let mut inner_pad = [0x36; 64];
        let mut outer_pad = [0x5c; 64];
        for (i, byte) in self.0.iter().enumerate() {
            inner_pad[i] ^= byte;
            outer_pad[i] ^= byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        inner.update(format!("v1\n{command}\n{incident}\n{expires}"));
        let mut outer = Sha256::new();
        outer.update(outer_pad);
        outer.update(inner.finalize());
        hex::encode(outer.finalize())
    }

    pub fn check(&self, command: &str, incident: &str, expires: i64, signature: &str, now: f64) -> Grant {
        let expected = self.sign(command, incident, expires);
        if !crate::monitor::constant_time_eq(signature.as_bytes(), expected.as_bytes())
            || expires as f64 > now + MAX_LIFETIME_S
        {
            Grant::Invalid
        } else if (expires as f64) < now {
            Grant::Expired
        } else {
            Grant::Valid
        }
    }

    pub fn verify(&self, command: &str, incident: &str, expires: i64, signature: &str, now: f64) -> bool {
        self.check(command, incident, expires, signature, now) == Grant::Valid
    }

    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                return bytes
                    .try_into()
                    .map(Self)
                    .map_err(|_| std::io::Error::other("invalid control capability key"));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let signer = Self::default();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(&signer.0)?;
                file.sync_all()?;
                Ok(signer)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Self::load(path),
            Err(error) => Err(error),
        }
    }
}
