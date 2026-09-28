//! Download Obico's open-source failure-detection model (ONNX).
//!
//! Source of truth for the URL: obico-server ml_api/model/model-weights.onnx.url
//! (AGPL-3.0, https://github.com/TheSpaghettiDetective/obico-server).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub const DEFAULT_URL: &str = "https://tsd-pub-static.s3.amazonaws.com/ml-models/model-weights-5a6b1be1fa.onnx";
/// the real model is ~200 MB; anything tiny is an error page
pub const MIN_BYTES: u64 = 1_000_000;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ModelDownloadError(pub String);

/// Something that yields the model bytes (HTTP in production, canned data in tests).
pub trait Fetch {
    /// Returns (reader, content-length if known).
    fn get(&self, url: &str) -> Result<(Box<dyn Read>, Option<u64>), String>;
}

pub struct HttpFetch;

impl Fetch for HttpFetch {
    fn get(&self, url: &str) -> Result<(Box<dyn Read>, Option<u64>), String> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(60))
            // blocking client: per read/write operation, not the whole download
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client.get(url).send().map_err(|e| crate::http::describe(&e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or("")));
        }
        let len = resp.content_length();
        Ok((Box::new(resp), len))
    }
}

/// Download the model to `out` (atomically, via a .part file). No-op if it exists and !force.
pub fn download_model(
    out: &Path,
    url: &str,
    force: bool,
    fetch: &dyn Fetch,
    progress: bool,
    verify_onnx: bool,
) -> Result<PathBuf, ModelDownloadError> {
    if out.exists() && !force {
        return Ok(out.to_path_buf());
    }
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| ModelDownloadError(format!("model download failed: {e}")))?;
    }
    let mut tmp_name = out.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".part");
    let tmp = out.with_file_name(tmp_name);
    eprintln!("Downloading model from {url}");
    let fail = |tmp: &Path, msg: String| {
        let _ = std::fs::remove_file(tmp);
        ModelDownloadError(msg)
    };
    let (mut reader, total) = fetch.get(url).map_err(|e| fail(&tmp, format!("model download failed: {e}")))?;
    let mut file = std::fs::File::create(&tmp).map_err(|e| fail(&tmp, format!("model download failed: {e}")))?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                if progress {
                    eprintln!();
                }
                return Err(fail(&tmp, format!("model download failed: {e}")));
            }
        };
        file.write_all(&buf[..n]).map_err(|e| fail(&tmp, format!("model download failed: {e}")))?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if progress && let Some(t) = total.filter(|t| *t > 0) {
            eprint!("\r  {:6.1} / {:.1} MB", done as f64 / 1e6, t as f64 / 1e6);
        }
    }
    if progress {
        eprintln!();
    }
    drop(file);
    if done < MIN_BYTES {
        return Err(fail(&tmp, format!("model download looks wrong ({done} bytes); aborting")));
    }
    if verify_onnx && let Err(e) = crate::detector::SpaghettiDetector::load(&tmp, false) {
        return Err(fail(&tmp, format!("downloaded file is not a loadable ONNX model: {e}")));
    }
    std::fs::rename(&tmp, out).map_err(|e| fail(&tmp, format!("model download failed: {e}")))?;
    eprintln!("Saved {} ({:.1} MB, sha256={})", out.display(), done as f64 / 1e6, hex::encode(hasher.finalize()));
    Ok(out.to_path_buf())
}
