//! Buddy3D camera frame source.
//!
//! The Buddy3D exposes an RTSP stream at `rtsp://<camera-ip>/live` once "RTSP"
//! is selected as the camera's streaming mode in the Prusa app / Prusa Connect.
//! There is no snapshot endpoint, so we hold one persistent RTSP session open
//! and always keep only the most recent decoded frame. Opening a fresh RTSP
//! session per inference would cost 1-3 s of handshake + keyframe wait and
//! hammer the camera's small SoC.
//!
//! Decoding is done by an `ffmpeg` child process (the same FFmpeg the Python
//! version used through OpenCV), which emits a couple of raw RGB frames per
//! second as a PPM stream. Any source ffmpeg can open works (file path, http
//! MJPEG, other RTSP cameras); files are looped, which is how tests and demos
//! run without hardware. Set `PRUSA_WATCH_FFMPEG` to use a specific binary.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use image::RgbImage;

use crate::now_ts;

#[derive(Clone)]
pub struct Frame {
    /// Increases once per decoded frame, including across reconnects.
    pub sequence: u64,
    pub image: Arc<RgbImage>,
    /// unix time when decoded
    pub ts: f64,
    pub monotonic_ts: f64,
}

/// What the monitor needs from a camera (the ffmpeg grabber, or a fake in tests).
pub trait FrameSource: Send + Sync {
    fn start(&self);
    fn stop(&self);
    fn latest(&self) -> Option<Frame>;
    fn connected(&self) -> bool;
}

/// Frames per second requested from ffmpeg. Detection runs every 10 s; 2 fps
/// keeps the newest frame at most ~0.5 s old without shipping every frame.
const OUTPUT_FPS: u32 = 2;

pub fn ffmpeg_bin() -> String {
    std::env::var("PRUSA_WATCH_FFMPEG")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ffmpeg".into())
}

struct Shared {
    latest: Mutex<Option<Frame>>,
    connected: AtomicBool,
    stop: AtomicBool,
    wake: (Mutex<()>, Condvar),
    child: Mutex<Option<Child>>,
    frames_decoded: AtomicU64,
    reconnects: AtomicU64,
}

pub struct FfmpegGrabber {
    pub url: String,
    pub transport: String,
    pub reconnect_backoff_s: f64,
    pub open_timeout_s: f64,
    pub read_timeout_s: f64,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl FfmpegGrabber {
    pub fn new(url: &str, transport: &str, reconnect_backoff_s: f64, open_timeout_s: f64, read_timeout_s: f64) -> Self {
        Self {
            url: url.into(),
            transport: transport.into(),
            reconnect_backoff_s,
            open_timeout_s,
            read_timeout_s,
            shared: Arc::new(Shared {
                latest: Mutex::new(None),
                connected: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                wake: (Mutex::new(()), Condvar::new()),
                child: Mutex::new(None),
                frames_decoded: AtomicU64::new(0),
                reconnects: AtomicU64::new(0),
            }),
            thread: Mutex::new(None),
        }
    }

    pub fn frames_decoded(&self) -> u64 {
        self.shared.frames_decoded.load(Ordering::Relaxed)
    }

    pub fn reconnects(&self) -> u64 {
        self.shared.reconnects.load(Ordering::Relaxed)
    }

    fn is_live(url: &str) -> bool {
        let u = url.to_lowercase();
        ["rtsp://", "rtsps://", "http://", "https://"]
            .iter()
            .any(|p| u.starts_with(p))
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(ffmpeg_bin());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        let lower = self.url.to_lowercase();
        if lower.starts_with("rtsp") {
            if !self.transport.is_empty() {
                cmd.args(["-rtsp_transport", &self.transport]);
            }
            // Timeouts are enforced by our watchdog (killing ffmpeg), not ffmpeg's own
            // flags: `-timeout` means "listen" on ffmpeg 4.x and a socket timeout on 5+.
        } else if !Self::is_live(&self.url) {
            // File source: play at native speed, forever (useful for demos/tests).
            cmd.args(["-re", "-stream_loop", "-1"]);
        }
        cmd.args([
            "-i",
            &self.url,
            "-an",
            "-sn",
            "-vf",
            &format!("fps={OUTPUT_FPS}"),
            "-f",
            "image2pipe",
            "-c:v",
            "ppm",
            "-pix_fmt",
            "rgb24",
            "-",
        ]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd
    }

    fn wait(shared: &Shared, secs: f64) {
        let (lock, cv) = &shared.wake;
        let guard = lock.lock().unwrap();
        let _ = cv.wait_timeout_while(guard, Duration::from_secs_f64(secs.max(0.0)), |_| {
            !shared.stop.load(Ordering::Relaxed)
        });
    }

    fn run(self: Arc<Self>) {
        let sh = self.shared.clone();
        let url = redact(&self.url);
        while !sh.stop.load(Ordering::Relaxed) {
            let mut child = match self.command().spawn() {
                Ok(c) => c,
                Err(e) => {
                    sh.connected.store(false, Ordering::Relaxed);
                    tracing::error!(
                        "Camera: could not start ffmpeg ({e}). Install it (macOS: brew install ffmpeg, Debian/Ubuntu: apt install ffmpeg) \
                         or set PRUSA_WATCH_FFMPEG; retrying in {:.0}s",
                        self.reconnect_backoff_s
                    );
                    sh.reconnects.fetch_add(1, Ordering::Relaxed);
                    Self::wait(&sh, self.reconnect_backoff_s);
                    continue;
                }
            };
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            *sh.child.lock().unwrap() = Some(child);
            if sh.stop.load(Ordering::Relaxed) {
                // stop() ran between spawn and here: nobody else will kill this one
                if let Some(c) = sh.child.lock().unwrap().as_mut() {
                    let _ = c.kill();
                }
            }

            let errors: Arc<Mutex<Vec<String>>> = Arc::default();
            let err_sink = errors.clone();
            let err_thread = std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut e = err_sink.lock().unwrap();
                    if e.len() < 20 {
                        e.push(line);
                    }
                }
            });

            // Watchdog: kill ffmpeg if no frame arrives in time (open, then read timeout).
            let last_frame = Arc::new(Mutex::new(None::<Instant>));
            let started = Instant::now();
            let wd_done = Arc::new(AtomicBool::new(false));
            let wd = {
                let (sh, last_frame, wd_done) = (sh.clone(), last_frame.clone(), wd_done.clone());
                let (open_t, read_t) = (self.open_timeout_s, self.read_timeout_s);
                std::thread::spawn(move || {
                    while !wd_done.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(200));
                        // on stop, kill ffmpeg too so a reader blocked on a hung stream wakes up
                        let stale = sh.stop.load(Ordering::Relaxed)
                            || match *last_frame.lock().unwrap() {
                                None => started.elapsed().as_secs_f64() > open_t + read_t,
                                Some(t) => t.elapsed().as_secs_f64() > read_t,
                            };
                        if stale {
                            if let Some(c) = sh.child.lock().unwrap().as_mut() {
                                let _ = c.kill();
                            }
                            break;
                        }
                    }
                })
            };

            let mut reader = BufReader::with_capacity(1 << 20, stdout);
            let mut got_any = false;
            // EOF, a read error, or a watchdog kill all end this session
            while let Ok(Some(img)) = read_ppm(&mut reader) {
                if !got_any {
                    got_any = true;
                    sh.connected.store(true, Ordering::Relaxed);
                    tracing::info!("Camera: connected to {url}");
                }
                *last_frame.lock().unwrap() = Some(Instant::now());
                let sequence = sh.frames_decoded.fetch_add(1, Ordering::Relaxed) + 1;
                *sh.latest.lock().unwrap() = Some(Frame {
                    sequence,
                    image: Arc::new(img),
                    ts: now_ts(),
                    monotonic_ts: crate::monotonic_ts(),
                });
                if sh.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            wd_done.store(true, Ordering::Relaxed);
            if let Some(mut c) = sh.child.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            let _ = wd.join();
            let _ = err_thread.join();
            sh.connected.store(false, Ordering::Relaxed);
            if sh.stop.load(Ordering::Relaxed) {
                break;
            }
            let detail = errors
                .lock()
                .unwrap()
                .last()
                .cloned()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default();
            if got_any {
                tracing::warn!("Camera: stream read failed{detail}, reconnecting");
            } else {
                tracing::warn!(
                    "Camera: could not open {url}{detail}; retrying in {:.0}s",
                    self.reconnect_backoff_s
                );
            }
            sh.reconnects.fetch_add(1, Ordering::Relaxed);
            Self::wait(&sh, self.reconnect_backoff_s);
        }
    }
}

/// Handle so the grabber (which needs `Arc<Self>` for its thread) can implement FrameSource.
pub struct Grabber(pub Arc<FfmpegGrabber>);

impl Grabber {
    pub fn new(url: &str, transport: &str, reconnect_backoff_s: f64, open_timeout_s: f64, read_timeout_s: f64) -> Self {
        Self(Arc::new(FfmpegGrabber::new(
            url,
            transport,
            reconnect_backoff_s,
            open_timeout_s,
            read_timeout_s,
        )))
    }
}

impl FrameSource for Grabber {
    fn start(&self) {
        let mut t = self.0.thread.lock().unwrap();
        if t.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        self.0.shared.stop.store(false, Ordering::Relaxed);
        let me = self.0.clone();
        *t = Some(
            std::thread::Builder::new()
                .name("frame-grabber".into())
                .spawn(move || me.run())
                .expect("spawn grabber"),
        );
    }

    fn stop(&self) {
        let sh = &self.0.shared;
        sh.stop.store(true, Ordering::Relaxed);
        {
            let _g = sh.wake.0.lock().unwrap(); // no lost wakeup for a thread about to wait
            sh.wake.1.notify_all();
        }
        if let Some(c) = sh.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
        if let Some(h) = self.0.thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }

    fn latest(&self) -> Option<Frame> {
        self.0.shared.latest.lock().unwrap().clone()
    }

    fn connected(&self) -> bool {
        self.0.shared.connected.load(Ordering::Relaxed)
    }
}

/// Read one binary PPM (P6, maxval 255) from the stream. Ok(None) at clean EOF.
pub fn read_ppm<R: BufRead>(r: &mut R) -> std::io::Result<Option<RgbImage>> {
    fn token<R: BufRead>(r: &mut R) -> std::io::Result<Option<String>> {
        let mut tok = Vec::new();
        loop {
            let mut b = [0u8; 1];
            if r.read(&mut b)? == 0 {
                return Ok(if tok.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&tok).into_owned())
                });
            }
            match b[0] {
                b'#' if tok.is_empty() => {
                    let mut line = Vec::new();
                    r.read_until(b'\n', &mut line)?;
                }
                c if c.is_ascii_whitespace() => {
                    if !tok.is_empty() {
                        return Ok(Some(String::from_utf8_lossy(&tok).into_owned()));
                    }
                }
                c => tok.push(c),
            }
        }
    }
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let Some(magic) = token(r)? else { return Ok(None) };
    if magic != "P6" {
        return Err(bad("not a P6 PPM"));
    }
    let mut num = || -> std::io::Result<u32> {
        token(r)?
            .ok_or_else(|| bad("truncated header"))?
            .parse()
            .map_err(|_| bad("bad header"))
    };
    let (w, h, max) = (num()?, num()?, num()?);
    if max != 255 || w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(bad("unsupported PPM"));
    }
    let mut buf = vec![0u8; (w * h * 3) as usize];
    r.read_exact(&mut buf)?;
    Ok(RgbImage::from_raw(w, h, buf))
}

/// rtsp://user:pass@host/... -> rtsp://***@host/...
pub fn redact(url: &str) -> String {
    if let (Some((scheme, rest)), true) = (url.split_once("://"), url.contains('@'))
        && let Some((_, host)) = rest.split_once('@')
    {
        return format!("{scheme}://***@{host}");
    }
    url.to_string()
}
