//! Port of tests/test_decision.py: Obico-style failure decision logic.

use std::collections::BTreeSet;

use prusa_watch::config::DecisionConfig;
use prusa_watch::decision::{FailureDecider, Verdict};

fn run(dec: &mut FailureDecider, ps: &[f64]) -> Vec<Verdict> {
    ps.iter().map(|p| dec.update(&[*p])).collect()
}

fn rep(v: f64, n: usize) -> Vec<f64> {
    vec![v; n]
}

fn distinct(v: &[Verdict]) -> BTreeSet<&'static str> {
    v.iter().map(Verdict::as_str).collect()
}

fn index(v: &[Verdict], x: Verdict) -> usize {
    v.iter()
        .position(|y| *y == x)
        .unwrap_or_else(|| panic!("{x:?} not in verdicts"))
}

fn cfg_with(f: impl FnOnce(&mut DecisionConfig)) -> DecisionConfig {
    let mut c = DecisionConfig::default();
    f(&mut c);
    c
}

#[test]
fn grace_period_blocks_early_triggers() {
    let mut dec = FailureDecider::new(DecisionConfig::default(), None);
    let verdicts = run(&mut dec, &rep(5.0, 29));
    assert_eq!(distinct(&verdicts), BTreeSet::from(["ok"]));
}

#[test]
fn clean_print_stays_ok() {
    let mut dec = FailureDecider::new(DecisionConfig::default(), None);
    let seq: Vec<f64> = [0.02, 0.0, 0.05, 0.01].repeat(60);
    let verdicts = run(&mut dec, &seq);
    assert_eq!(distinct(&verdicts), BTreeSet::from(["ok"]));
    assert!(dec.state.normalized_p < 1.0 / 3.0);
}

#[test]
fn spaghetti_escalates_warning_then_failure() {
    let mut dec = FailureDecider::new(DecisionConfig::default(), None);
    run(&mut dec, &rep(0.02, 60)); // 10 clean minutes
    let verdicts = run(&mut dec, &rep(2.5, 20)); // model starts seeing lots of spaghetti
    assert!(verdicts.contains(&Verdict::Warning));
    assert!(verdicts.contains(&Verdict::Failure));
    assert!(index(&verdicts, Verdict::Warning) < index(&verdicts, Verdict::Failure));
    // sustained spaghetti should pause within ~2 minutes (12 frames at 10 s)
    assert!(index(&verdicts, Verdict::Failure) <= 12);
}

#[test]
fn single_frame_blip_does_not_pause() {
    let mut dec = FailureDecider::new(DecisionConfig::default(), None);
    run(&mut dec, &rep(0.02, 60));
    let mut seq = vec![1.5];
    seq.extend(rep(0.02, 10));
    let verdicts = run(&mut dec, &seq);
    assert!(!verdicts.contains(&Verdict::Failure));
}

/// A camera angle that constantly produces p=0.6 must not trigger once the baseline learns it.
#[test]
fn long_baseline_absorbs_constant_camera_noise() {
    let mut dec = FailureDecider::new(DecisionConfig::default(), None);
    let mut verdicts = Vec::new();
    for _ in 0..5 {
        // five prints worth of history
        dec.reset_for_new_print();
        verdicts = run(&mut dec, &rep(0.6, 200));
    }
    assert!(!verdicts.contains(&Verdict::Failure));
    let l = dec.state.rolling_mean_long;
    assert!(0.4 < l && l < 0.6, "{l}"); // converging on the noise level
}

#[test]
fn sensitivity_scales_trigger() {
    let mut seq = rep(0.02, 60);
    seq.extend(rep(0.9, 30));
    let mut low = FailureDecider::new(cfg_with(|c| c.sensitivity = 0.5), None);
    let mut high = FailureDecider::new(cfg_with(|c| c.sensitivity = 2.0), None);
    assert!(!run(&mut low, &seq).contains(&Verdict::Failure));
    assert!(run(&mut high, &seq).contains(&Verdict::Failure));
}

#[test]
fn state_persists_and_reset_keeps_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.json");
    let mut dec = FailureDecider::new(cfg_with(|c| c.baseline_prior_frames = 0), Some(&path));
    run(&mut dec, &rep(0.3, 50));
    let base = dec.state.rolling_mean_long;
    let mut dec2 = FailureDecider::new(DecisionConfig::default(), Some(&path));
    assert_eq!(dec2.state.lifetime_frame_num, 50);
    assert_eq!(dec2.state.rolling_mean_long, base);
    dec2.reset_for_new_print();
    assert!(dec2.state.current_frame_num == 0 && dec2.state.ewm_mean == 0.0);
    assert_eq!(dec2.state.rolling_mean_long, base); // baseline survives new prints
}

#[test]
fn corrupt_state_file_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.json");
    std::fs::write(&path, "{not json").unwrap();
    let dec = FailureDecider::new(DecisionConfig::default(), Some(&path));
    assert_eq!(dec.state.lifetime_frame_num, 360); // fresh state gets the clean prior
}

/// Hand-check one step against Obico's formulas (prior off = exact Obico).
#[test]
fn matches_obico_reference_math() {
    let mut dec = FailureDecider::new(cfg_with(|c| c.baseline_prior_frames = 0), None);
    dec.update(&[0.5, 0.25]); // p = 0.75
    let alpha = 2.0 / (12.0 + 1.0);
    assert!((dec.state.ewm_mean - 0.75 * alpha).abs() < 1e-9);
    // first frame: rolling mean = 0 + (0.75 - 0)/ (1+1)
    assert!((dec.state.rolling_mean_short - 0.375).abs() < 1e-9);
    assert!((dec.state.rolling_mean_long - 0.375).abs() < 1e-9);
}

/// Pure Obico on a fresh install never pauses this: the baseline IS this print.
#[test]
fn fresh_install_catches_spaghetti_from_first_layer() {
    let seq = rep(2.5, 40);
    assert!(
        !run(
            &mut FailureDecider::new(cfg_with(|c| c.baseline_prior_frames = 0), None),
            &seq
        )
        .contains(&Verdict::Failure)
    );
    assert!(run(&mut FailureDecider::new(DecisionConfig::default(), None), &seq).contains(&Verdict::Failure));
}

#[test]
fn heavy_spaghetti_from_first_layer_pauses_at_end_of_grace() {
    let v = run(&mut FailureDecider::new(DecisionConfig::default(), None), &rep(2.5, 40));
    assert_eq!(index(&v, Verdict::Failure) + 1, 30);
}
