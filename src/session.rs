//! Printer session and intervention ownership. HTTP acceptance and observed state are distinct.
use serde::{Deserialize, Serialize};

use crate::escalation::Incident;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Pause,
    Resume,
    Stop,
}

impl Action {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pause" => Some(Self::Pause),
            "resume" => Some(Self::Resume),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
        }
    }

    pub fn result(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Resume => "resumed",
            Self::Stop => "stopped",
        }
    }

    pub fn confirmed(self, state: &str) -> bool {
        match self {
            Self::Pause => state == "PAUSED",
            Self::Resume => state == "PRINTING",
            Self::Stop => !["PRINTING", "PAUSED", "ATTENTION"].contains(&state),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobContext {
    pub job_id: Option<i64>,
    pub job_name: Option<String>,
    /// A local generation protects controls even when a printer reuses a numeric job ID.
    pub session_id: String,
    pub muted: bool,
    /// Confirmed interventions, retained as strings only for the existing dashboard wire format.
    pub action_taken: Option<String>,
    pub last_warning_ts: f64,
    pub warnings: i64,
    pub rearm_at: f64,
    pub printer_handled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingAction {
    pub action: Action,
    pub job_id: i64,
    pub requested_at: f64,
    pub deadline: f64,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionOutcome {
    Confirmed(Action),
    Requested(Action),
    Failed(String),
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Session {
    pub job: JobContext,
    pub incident: Option<Incident>,
    pub pending_action: Option<PendingAction>,
    pub action_error: Option<String>,
}

/// One versioned snapshot binds detector and control state to a printer and print.
#[derive(Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub config_digest: String,
    pub saved_at: f64,
    pub time_printing: Option<i64>,
    pub session: Session,
    pub prediction: crate::decision::PredictionState,
}

fn valid_id(id: &str) -> bool {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(id)
        .is_ok_and(|bytes| bytes.len() == 16)
}

impl Checkpoint {
    pub fn digest(cfg: &crate::config::Config) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(serde_json::to_vec(cfg).expect("serializable config")))
    }

    pub fn valid(&self, digest: &str, now: f64) -> bool {
        let job = &self.session.job;
        self.version == 1
            && self.config_digest == digest
            && self.prediction.is_valid()
            && self.saved_at.is_finite()
            && (0.0..=86400.0).contains(&(now - self.saved_at))
            && job.job_id.is_some_and(|id| id >= 0)
            && valid_id(&job.session_id)
            && job.warnings >= 0
            && [job.rearm_at, job.last_warning_ts].iter().all(|v| v.is_finite())
            && self.time_printing.is_none_or(|time| time >= 0)
            && self.session.pending_action.as_ref().is_none_or(|p| {
                Some(p.job_id) == job.job_id
                    && p.requested_at.is_finite()
                    && p.deadline.is_finite()
                    && p.deadline >= p.requested_at
                    && (1..=10).contains(&p.attempts)
            })
            && self.session.incident.as_ref().is_none_or(|i| {
                i.job_id == job.job_id
                    && i.started_ts.is_finite()
                    && i.started_ts <= self.saved_at
                    && valid_id(&i.id)
                    && i.score.is_finite()
                    && (0.0..=1.0).contains(&i.score)
                    && i.next_idx <= i.policy.steps.len()
                    && i.retry_idx.is_none_or(|n| n < i.policy.steps.len())
                    && i.policy.steps.iter().all(|step| {
                        step.at.is_finite()
                            && (0.0..=604800.0).contains(&step.at)
                            && step
                                .action
                                .as_deref()
                                .is_none_or(|action| matches!(action, "pause" | "stop"))
                    })
            })
    }
}

impl Session {
    /// Translate deadlines between an elapsed-time clock and persisted UTC.
    pub fn shift_time(&mut self, delta: f64) {
        for time in [&mut self.job.rearm_at, &mut self.job.last_warning_ts] {
            if *time > 0.0 {
                *time += delta;
            }
        }
        if let Some(i) = &mut self.incident {
            i.started_ts += delta;
        }
        if let Some(p) = &mut self.pending_action {
            p.requested_at += delta;
            p.deadline += delta;
        }
    }
    pub fn replace_job(&mut self, job_id: Option<i64>, name: Option<String>) {
        *self = Self {
            job: JobContext {
                job_id,
                job_name: name,
                session_id: crate::escalation::new_incident_id(),
                ..Default::default()
            },
            ..Default::default()
        };
    }

    pub fn accepts(&self, job_id: i64, session_id: &str) -> bool {
        self.job.job_id == Some(job_id) && self.job.session_id == session_id
    }

    pub fn request(&mut self, action: Action, job_id: i64, now: f64, timeout: f64, attempts: u32) {
        self.pending_action = Some(PendingAction {
            action,
            job_id,
            requested_at: now,
            deadline: now + timeout,
            attempts,
        });
        self.action_error = None;
    }

    /// Returns the observed action; never promotes an accepted-but-unobserved request.
    pub fn reconcile(&mut self, job_id: Option<i64>, state: &str) -> Option<Action> {
        let pending = self.pending_action.as_ref()?;
        if Some(pending.job_id) != job_id || !pending.action.confirmed(state) {
            return None;
        }
        let action = pending.action;
        self.pending_action = None;
        self.action_error = None;
        if action != Action::Resume {
            self.job.action_taken = Some(action.result().into());
            if let Some(incident) = &mut self.incident {
                incident.acted = self.job.action_taken.clone();
            }
        }
        Some(action)
    }

    pub fn mute(&mut self, muted: bool) {
        self.job.muted = muted;
        if muted {
            self.incident = None;
            // A sent request can still complete, but may not be retried after the user's veto.
            self.pending_action = None;
            self.action_error = None;
        }
    }
}
