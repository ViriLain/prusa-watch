//! Expiring, command-specific links never carry the dashboard's shared control token.
use sha2::{Digest, Sha256};

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

    pub fn verify(&self, command: &str, incident: &str, expires: i64, signature: &str, now: f64) -> bool {
        expires as f64 >= now
            && expires as f64 <= now + 86400.0
            && crate::monitor::constant_time_eq(signature.as_bytes(), self.sign(command, incident, expires).as_bytes())
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
