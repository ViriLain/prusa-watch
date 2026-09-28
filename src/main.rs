//! prusa-watch CLI.
//!
//! ```text
//! prusa-watch run    [-c config.yaml]   # start monitor + dashboard
//! prusa-watch check  [-c config.yaml]   # verify printer, camera, model and escalation, then exit
//! prusa-watch config [-c config.yaml]   # print the effective config (defaults + file + env), secrets masked
//! prusa-watch config --defaults         # print the built-in defaults only
//! prusa-watch fetch-model [--force]     # download the detection model (run/check do this if it's missing)
//! prusa-watch report [JOB_ID]           # what the detector saw on a recorded print (default: latest)
//! ```
//!
//! A `.env` next to the config file (or in the current directory) is loaded
//! automatically; variables already set in the shell win.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use prusa_watch::camera::{FrameSource, Grabber};
use prusa_watch::config::{Config, ConfigError, effective_config, load_config, process_env};
use prusa_watch::detector::{Detect, SpaghettiDetector};
use prusa_watch::escalation::{PolicyResolver, parse_escalation};
use prusa_watch::imaging::{crop_roi, encode_jpeg};
use prusa_watch::model::{DEFAULT_URL, HttpFetch, download_model};
use prusa_watch::monitor::{Monitor, Parts};
use prusa_watch::prusalink::{Printer, PrusaLink};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Cmd {
    Run,
    Check,
    Config,
    FetchModel,
    Report,
}

#[derive(Parser)]
#[command(name = "prusa-watch", version, about = "AI spaghetti detection for Prusa printers + Buddy3D camera")]
struct Args {
    #[arg(value_enum, default_value = "run")]
    command: Cmd,
    /// report: job id (default: the most recent recorded print)
    job: Option<String>,
    #[arg(short, long, env = "PRUSA_WATCH_CONFIG", default_value = "config.yaml")]
    config: PathBuf,
    /// load this .env instead of looking next to the config / in the cwd
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// config: show built-in defaults only
    #[arg(long)]
    defaults: bool,
    /// config: don't mask passwords/tokens/topics
    #[arg(long)]
    show_secrets: bool,
    /// fetch-model: re-download even if present
    #[arg(long)]
    force: bool,
}

fn setup_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let lvl = match level.to_uppercase().as_str() {
        "DEBUG" => "debug",
        "WARNING" | "WARN" => "warn",
        "ERROR" | "CRITICAL" => "error",
        _ => "info",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(format!("{lvl},hyper=warn,reqwest=warn,tract=warn")));
    let timer = tracing_subscriber::fmt::time::ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".into());
    use std::io::IsTerminal;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(timer)
        .with_target(true)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
}

fn load_env_files(args: &Args) -> Result<(), ExitCode> {
    let candidates = match &args.env_file {
        Some(p) => {
            if !p.is_file() {
                eprintln!("--env-file {}: not found", p.display());
                return Err(ExitCode::from(2));
            }
            vec![p.clone()]
        }
        None => prusa_watch::dotenv::default_candidates(&args.config),
    };
    for p in prusa_watch::dotenv::load_dotenv(&candidates, &mut prusa_watch::dotenv::ProcessEnv) {
        eprintln!("prusa-watch: loaded {}", p.display());
    }
    Ok(())
}

fn ensure_model(path: &str, force: bool) -> bool {
    if Path::new(path).exists() && !force {
        return true;
    }
    match download_model(Path::new(path), DEFAULT_URL, force, &HttpFetch, true, true) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("{e}\nRetry with:  prusa-watch fetch-model");
            false
        }
    }
}

fn cmd_check(cfg: &Config) -> ExitCode {
    let mut ok = true;
    let p = &cfg.printer;
    println!("[printer] {} (auth={})", p.host, p.auth);
    let pl = PrusaLink::new(&p.host, &p.password, &p.username, &p.auth, &p.scheme, 5.0);
    match pl.status() {
        Ok(st) => {
            let o = |v: Option<f64>| v.map(|x| format!("{x:?}")).unwrap_or("None".into());
            println!(
                "  OK  state={} job_id={} nozzle={} bed={}",
                st.state,
                st.job_id.map(|j| j.to_string()).unwrap_or("None".into()),
                o(st.temp_nozzle),
                o(st.temp_bed)
            );
            if st.job_id.is_some() {
                println!("  job: {}", pl.job_name().unwrap_or("None".into()));
            }
        }
        Err(e) => {
            ok = false;
            println!("  FAIL {e}");
        }
    }

    println!("[camera] {}", cfg.camera.url);
    let c = &cfg.camera;
    let g = Grabber::new(&c.url, &c.transport, c.reconnect_backoff_s, c.open_timeout_s, c.read_timeout_s);
    g.start();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && g.latest().is_none() {
        std::thread::sleep(Duration::from_millis(250));
    }
    let frame = g.latest();
    g.stop();
    match &frame {
        None => {
            ok = false;
            println!("  FAIL no frame within 20 s. Is RTSP enabled for the camera in the Prusa app? Is the IP right?");
        }
        Some(f) => {
            println!("  OK  {}x{}", f.image.width(), f.image.height());
            let path = Path::new(&cfg.state_dir).join("check_frame.jpg");
            let _ = std::fs::create_dir_all(&cfg.state_dir);
            match std::fs::write(&path, encode_jpeg(&f.image, 95)) {
                Ok(()) => println!("  saved {}", path.display()),
                Err(e) => println!("  (could not save {}: {e})", path.display()),
            }
        }
    }

    println!("[escalation]");
    let esc = parse_escalation(Some(&cfg.escalation), true).expect("validated");
    let resolver = PolicyResolver::new(esc.clone(), &cfg.timezone).expect("validated");
    let (pol, sched) = resolver.resolve(prusa_watch::now_ts());
    for (name, p) in &esc.policies {
        let mut marks = if *name == esc.default_policy { " (default)".to_string() } else { String::new() };
        if *name == pol.name {
            marks += " <- active now";
            if let Some(s) = &sched {
                marks += &format!(" via schedule '{s}'");
            }
        }
        println!("  policy {name}{marks}");
        for st in &p.steps {
            let notify = match &st.notify {
                None => "all".to_string(),
                Some(n) => format!("[{}]", n.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ")),
            };
            println!(
                "    at {:>6.0}s  action={:<5}  notify={notify}  prio={}",
                st.at,
                st.action.as_deref().unwrap_or("-"),
                st.priority
            );
        }
    }
    let n = &cfg.notify;
    if !n.ntfy.topic.is_empty() && n.ntfy.reply_topic.is_empty() {
        println!("  note: notify.ntfy.reply_topic not set - buttons only work on your LAN (via web.public_url)");
    }
    if n.ntfy.topic.is_empty() && n.discord.webhook_url.is_empty() && n.webhook.url.is_empty() {
        println!("  WARNING: no notification channel configured - policies that wait for your answer will just act late");
    }

    println!("[model] {}", cfg.detector.model_path);
    if !ensure_model(&cfg.detector.model_path, false) {
        ok = false;
        println!("  FAIL model missing and download failed");
    } else {
        match SpaghettiDetector::load(&cfg.detector.model_path, cfg.detector.use_gpu) {
            Ok(det) => match &frame {
                Some(f) => {
                    let img = crop_roi(&f.image, cfg.camera.roi.as_deref());
                    let dets = det.detect(&img, cfg.detector.threshold, cfg.detector.nms);
                    let sum: f64 = dets.iter().map(|d| d.confidence).fold(0.0, |a, b| a + b);
                    println!("  OK  {} boxes, sum p={sum:.3}, {:.0} ms", dets.len(), det.last_inference_ms());
                }
                None => println!("  OK  loaded"),
            },
            Err(e) => {
                ok = false;
                println!("  FAIL {e}");
            }
        }
    }
    println!("{}", if ok { "\nAll checks passed." } else { "\nSome checks failed." });
    if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

fn cmd_run(cfg: Config) -> ExitCode {
    if !ensure_model(&cfg.detector.model_path, false) {
        return ExitCode::from(1);
    }
    let web = cfg.web.clone();
    let monitor = match Monitor::new(cfg, Parts::default()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    monitor.start();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let res = rt.block_on(async {
        if web.enabled {
            tracing::info!("Dashboard on http://{}:{}", web.host, web.port);
            prusa_watch::web::serve(monitor.clone(), &web.host, web.port).await
        } else {
            prusa_watch::web::shutdown_signal().await;
            Ok(())
        }
    });
    let m = monitor.clone();
    let _ = std::thread::spawn(move || m.stop()).join();
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_config(args: &Args) -> ExitCode {
    let cfg = if args.defaults {
        Config::default()
    } else {
        match load_config(Some(&args.config), &process_env()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        }
    };
    print!("{}", serde_yaml::to_string(&effective_config(&cfg, !args.show_secrets)).unwrap_or_default());
    if !args.defaults
        && let Err(e) = cfg.validate()
    {
        eprintln!("{}", format!("\n# {e}").replace('\n', "\n# "));
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

fn config_if_present(args: &Args) -> Result<Config, ExitCode> {
    if args.config.exists() {
        load_config(Some(&args.config), &process_env()).map_err(|e| {
            eprintln!("{e}");
            ExitCode::from(2)
        })
    } else {
        Ok(Config::default())
    }
}

fn cmd_fetch_model(args: &Args) -> ExitCode {
    let path = match config_if_present(args) {
        Ok(c) => c.detector.model_path,
        Err(code) => return code,
    };
    let p = Path::new(&path);
    if p.exists() && !args.force {
        let mb = std::fs::metadata(p).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
        println!("{path} already exists ({mb:.1} MB); use --force to re-download");
        return ExitCode::SUCCESS;
    }
    if ensure_model(&path, args.force) { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

fn cmd_report(args: &Args) -> ExitCode {
    let state_dir = match config_if_present(args) {
        Ok(c) => PathBuf::from(c.state_dir),
        Err(code) => return code,
    };
    let hist = state_dir.join("history");
    let path = match &args.job {
        Some(j) => Some(hist.join(format!("job-{j}.csv"))),
        None => std::fs::read_dir(&hist)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|x| x == "csv")
                    && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("job-"))
            })
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()),
    };
    let Some(path) = path.filter(|p| p.exists()) else {
        eprintln!(
            "No recorded print found in {}{}",
            hist.display(),
            args.job.as_ref().map(|j| format!(" for job {j}")).unwrap_or_default()
        );
        return ExitCode::from(1);
    };
    let s = match prusa_watch::recording::summarize(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return ExitCode::from(1);
        }
    };
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let name = std::fs::read_to_string(path.with_extension("json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("job_name").and_then(|n| n.as_str().map(str::to_string)))
        .unwrap_or_default();
    println!("{stem}  {name}");
    if s.frames == 0 {
        println!("  no analyzed frames");
        return ExitCode::SUCCESS;
    }
    let v = |k: &str| s.verdicts.iter().find(|(n, _)| n == k).map(|(_, c)| *c).unwrap_or(0);
    println!("  frames      {}  ({} .. {})", s.frames, s.from, s.to);
    println!("  verdicts    ok {}  warning {}  failure {}", v("ok"), v("warning"), v("failure"));
    println!("  peak p      {:.2} at {}  (summed box confidence per frame)", s.peak_p, s.peak_p_at);
    println!("  peak score  {:.2}  (1/3 = warning line, 2/3 = pause line)", s.peak_score);
    println!("  baseline    {:.3}", s.baseline);
    if let Some(t) = &s.first_warning {
        println!("  first warn  {t}");
    }
    if let Some(t) = &s.first_failure {
        println!("  first fail  {t}");
    }
    println!("  frames saved {} -> {}", s.frames_saved, state_dir.join("frames").join(&stem).display());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Err(code) = load_env_files(&args) {
        return code;
    }
    match args.command {
        Cmd::Config => return cmd_config(&args),
        Cmd::FetchModel => return cmd_fetch_model(&args),
        Cmd::Report => return cmd_report(&args),
        _ => {}
    }
    let cfg = match load_config(Some(&args.config), &process_env()) {
        Ok(c) => c,
        Err(ConfigError::NotFound(p)) => {
            eprintln!("Config file not found: {p}\nCreate one with:  cp config.example.yaml {}", args.config.display());
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    setup_logging(&cfg.log_level);
    if let Err(e) = cfg.validate() {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    match args.command {
        Cmd::Check => cmd_check(&cfg),
        _ => cmd_run(cfg),
    }
}
