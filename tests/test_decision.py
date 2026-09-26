from prusa_watch.config import DecisionConfig
from prusa_watch.decision import FailureDecider, Verdict


def run(dec, ps):
    return [dec.update([p]) for p in ps]


def test_grace_period_blocks_early_triggers():
    dec = FailureDecider(DecisionConfig())
    verdicts = run(dec, [5.0] * 29)
    assert set(verdicts) == {Verdict.OK}


def test_clean_print_stays_ok():
    dec = FailureDecider(DecisionConfig())
    verdicts = run(dec, [0.02, 0.0, 0.05, 0.01] * 60)
    assert set(verdicts) == {Verdict.OK}
    assert dec.state.normalized_p < 1 / 3


def test_spaghetti_escalates_warning_then_failure():
    dec = FailureDecider(DecisionConfig())
    run(dec, [0.02] * 60)  # 10 clean minutes
    verdicts = run(dec, [2.5] * 20)  # model starts seeing lots of spaghetti
    assert Verdict.WARNING in verdicts
    assert Verdict.FAILURE in verdicts
    assert verdicts.index(Verdict.WARNING) < verdicts.index(Verdict.FAILURE)
    # sustained spaghetti should pause within ~2 minutes (12 frames at 10 s)
    assert verdicts.index(Verdict.FAILURE) <= 12


def test_single_frame_blip_does_not_pause():
    dec = FailureDecider(DecisionConfig())
    run(dec, [0.02] * 60)
    verdicts = run(dec, [1.5] + [0.02] * 10)
    assert Verdict.FAILURE not in verdicts


def test_long_baseline_absorbs_constant_camera_noise():
    """A camera angle that constantly produces p=0.6 must not trigger once the baseline learns it."""
    dec = FailureDecider(DecisionConfig())
    for _ in range(5):  # five prints worth of history
        dec.reset_for_new_print()
        verdicts = run(dec, [0.6] * 200)
    assert Verdict.FAILURE not in verdicts
    assert 0.4 < dec.state.rolling_mean_long < 0.6  # converging on the noise level


def test_sensitivity_scales_trigger():
    seq = [0.02] * 60 + [0.9] * 30
    low = FailureDecider(DecisionConfig(sensitivity=0.5))
    high = FailureDecider(DecisionConfig(sensitivity=2.0))
    assert Verdict.FAILURE not in run(low, seq)
    assert Verdict.FAILURE in run(high, seq)


def test_state_persists_and_reset_keeps_baseline(tmp_path):
    path = tmp_path / "state.json"
    dec = FailureDecider(DecisionConfig(baseline_prior_frames=0), path)
    run(dec, [0.3] * 50)
    base = dec.state.rolling_mean_long
    dec2 = FailureDecider(DecisionConfig(), path)
    assert dec2.state.lifetime_frame_num == 50
    assert dec2.state.rolling_mean_long == base
    dec2.reset_for_new_print()
    assert dec2.state.current_frame_num == 0 and dec2.state.ewm_mean == 0
    assert dec2.state.rolling_mean_long == base  # baseline survives new prints


def test_corrupt_state_file_is_ignored(tmp_path):
    path = tmp_path / "state.json"
    path.write_text("{not json")
    dec = FailureDecider(DecisionConfig(), path)
    assert dec.state.lifetime_frame_num == 360  # fresh state gets the clean prior


def test_matches_obico_reference_math():
    """Hand-check one step against Obico's formulas (prior off = exact Obico)."""
    dec = FailureDecider(DecisionConfig(baseline_prior_frames=0))
    dec.update([0.5, 0.25])  # p = 0.75
    alpha = 2 / (12 + 1)
    assert abs(dec.state.ewm_mean - 0.75 * alpha) < 1e-9
    # first frame: rolling mean = 0 + (0.75 - 0)/ (1+1)
    assert abs(dec.state.rolling_mean_short - 0.375) < 1e-9
    assert abs(dec.state.rolling_mean_long - 0.375) < 1e-9


def test_fresh_install_catches_spaghetti_from_first_layer():
    """Pure Obico on a fresh install never pauses this: the baseline IS this print."""
    seq = [2.5] * 40
    assert Verdict.FAILURE not in run(FailureDecider(DecisionConfig(baseline_prior_frames=0)), seq)
    assert Verdict.FAILURE in run(FailureDecider(DecisionConfig()), seq)


def test_heavy_spaghetti_from_first_layer_pauses_at_end_of_grace():
    v = run(FailureDecider(DecisionConfig()), [2.5] * 40)
    assert v.index(Verdict.FAILURE) + 1 == 30
