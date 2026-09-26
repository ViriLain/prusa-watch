"""Failure decision logic.

Port of Obico's 1st-gen prediction algorithm (obico-server
``backend/lib/prediction.py`` + ``failure_detection.py``, AGPL-3.0).

Why not just "pause if any box > 0.5"? Single frames are noisy: purge lines,
brims, support trees, and the INDX tool dock can all light up for a frame or
two. Obico instead:

  * sums box confidences per frame -> p
  * tracks an exponential moving average of p (span 12 frames ~= 2 min)
  * subtracts a long-run per-printer baseline (rolling mean over 7200 frames
    ~= 20 h of printing) so a camera angle that always shows a little "noise"
    doesn't trigger
  * compares against a short rolling mean for the current print
  * gives 30 frames (5 min at 10 s) of grace at print start
  * requires the signal to be `escalating_factor` (1.75x) stronger to pause
    than to warn.

The long baseline persists across prints and restarts (saved to JSON).
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import asdict, dataclass
from enum import Enum
from pathlib import Path

from .config import DecisionConfig

log = logging.getLogger(__name__)


class Verdict(str, Enum):
    OK = "ok"
    WARNING = "warning"
    FAILURE = "failure"


@dataclass
class PredictionState:
    current_frame_num: int = 0
    lifetime_frame_num: int = 0
    current_p: float = 0.0
    ewm_mean: float = 0.0
    rolling_mean_short: float = 0.0
    rolling_mean_long: float = 0.0
    normalized_p: float = 0.0

    def reset_for_new_print(self) -> None:
        self.current_frame_num = 0
        self.current_p = 0.0
        self.ewm_mean = 0.0
        self.rolling_mean_short = 0.0
        self.normalized_p = 0.0


def _next_ewm(p: float, cur: float, alpha: float) -> float:
    return p * alpha + cur * (1 - alpha)


def _next_rolling(p: float, cur: float, count: int, win: int) -> float:
    return cur + (p - cur) / float(win if win <= count else count + 1)


class FailureDecider:
    def __init__(self, cfg: DecisionConfig, state_path: str | os.PathLike | None = None):
        self.cfg = cfg
        self.state_path = Path(state_path) if state_path else None
        self.state = self._load()

    # -- persistence ------------------------------------------------------
    def _load(self) -> PredictionState:
        if self.state_path and self.state_path.exists():
            try:
                data = json.loads(self.state_path.read_text())
                return PredictionState(**{k: v for k, v in data.items() if k in PredictionState.__annotations__})
            except Exception as exc:  # corrupt file shouldn't brick the monitor
                log.warning("Decision: could not load state (%s); starting fresh", exc)
        return PredictionState(lifetime_frame_num=max(0, self.cfg.baseline_prior_frames))

    def save(self) -> None:
        if not self.state_path:
            return
        self.state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.state_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(asdict(self.state), indent=2))
        tmp.replace(self.state_path)

    # -- algorithm --------------------------------------------------------
    def reset_for_new_print(self) -> None:
        self.state.reset_for_new_print()
        self.save()

    def update(self, confidences: list[float]) -> Verdict:
        c, s = self.cfg, self.state
        p = float(sum(confidences))
        alpha = 2 / (c.ewm_span + 1)
        s.current_p = p
        s.current_frame_num += 1
        s.lifetime_frame_num += 1
        s.ewm_mean = _next_ewm(p, s.ewm_mean, alpha)
        s.rolling_mean_short = _next_rolling(p, s.rolling_mean_short, s.current_frame_num, c.rolling_win_short)
        s.rolling_mean_long = _next_rolling(p, s.rolling_mean_long, s.lifetime_frame_num, c.rolling_win_long)
        s.normalized_p = self._normalized_p()
        self.save()

        if self._is_failing(c.escalating_factor):
            return Verdict.FAILURE
        if self._is_failing(1.0):
            return Verdict.WARNING
        return Verdict.OK

    def _is_failing(self, escalating_factor: float) -> bool:
        c, s = self.cfg, self.state
        if s.current_frame_num < c.init_safe_frame_num:
            return False
        adjusted = (s.ewm_mean - s.rolling_mean_long) * c.sensitivity / escalating_factor
        if adjusted < c.threshold_low:
            return False
        if adjusted > c.threshold_high:
            return True
        return adjusted > (s.rolling_mean_short - s.rolling_mean_long) * c.rolling_mean_short_multiple

    def _normalized_p(self) -> float:
        """0..1 score for display: <1/3 fine, 1/3-2/3 warning band, >2/3 failure band."""
        c, s = self.cfg, self.state

        def scale(v, lo, hi, nlo, nhi):
            if hi == lo:
                return nlo
            return min(nhi, max(nlo, ((v - lo) * (nhi - nlo)) / (hi - lo) + nlo))

        warn = (s.rolling_mean_short - s.rolling_mean_long) * c.rolling_mean_short_multiple
        warn = min(c.threshold_high, max(c.threshold_low, warn))
        fail = warn * c.escalating_factor
        p = (s.ewm_mean - s.rolling_mean_long) * c.sensitivity
        if p > fail:
            return scale(p, fail, fail * 1.5, 2 / 3, 1.0)
        if p > warn:
            return scale(p, warn, fail, 1 / 3, 2 / 3)
        return scale(p, 0, warn, 0, 1 / 3)
