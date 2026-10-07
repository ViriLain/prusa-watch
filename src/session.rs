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

    /// Whether a printer report is positive evidence that this action took effect on `job_id`.
    ///
    /// `observed_job` is the job the report names, if any, and `state` the printer state
    /// from the same report. An unknown, transitional or unrecognized state confirms
    /// nothing, and neither does a report about a different job: the only way to learn
    /// that a print stopped is for the printer to say so.
    pub fn confirmed(self, job_id: i64, observed_job: Option<i64>, state: &str) -> bool {
        match self {
            Self::Pause => observed_job == Some(job_id) && state == "PAUSED",
            Self::Resume => observed_job == Some(job_id) && state == "PRINTING",
            // STOPPED is the printer's own statement that the print was aborted. Buddy firmware
            // names no job in that state, so "no job" is accepted; another job's id is not.
            // FINISHED, IDLE and READY mean the print is over, not that it was stopped.
            Self::Stop => state == "STOPPED" && observed_job.is_none_or(|observed| observed == job_id),
        }
    }
}

/// States in which the printer reports that no print is running or held: the job is over,
/// however it ended. Only STOPPED says it was cancelled. BUSY and ERROR say nothing about
/// the job, and ATTENTION still holds one.
pub const ENDED_STATES: [&str; 4] = ["STOPPED", "FINISHED", "IDLE", "READY"];

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

/// Where to report the outcome of a request an escalation step made: that step's own
/// channels and priority. A request a person made has none and is reported everywhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// the step's `notify`: None = every channel, empty = silent
    pub notify: Option<Vec<String>>,
    pub priority: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingAction {
    pub action: Action,
    pub job_id: i64,
    pub requested_at: f64,
    pub deadline: f64,
    pub attempts: u32,
    #[serde(default)]
    pub route: Option<Route>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionOutcome {
    Confirmed(Action),
    Requested(Action),
    /// The print was over before the request was confirmed. Nothing was claimed, the
    /// request is no longer pending, and there is nothing left to retry.
    Ended(String),
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
        // A retry of the same request keeps reporting where the original would have.
        let route = self
            .pending_action
            .take()
            .filter(|pending| pending.action == action && pending.job_id == job_id)
            .and_then(|pending| pending.route);
        self.pending_action = Some(PendingAction {
            action,
            job_id,
            requested_at: now,
            deadline: now + timeout,
            attempts,
            route,
        });
        self.action_error = None;
    }

    /// Returns the observed action; never promotes an accepted-but-unobserved request.
    /// `observed_job` and `state` must come from the same printer report.
    pub fn reconcile(&mut self, observed_job: Option<i64>, state: &str) -> Option<Action> {
        let pending = self.pending_action.as_ref()?;
        if !pending.action.confirmed(pending.job_id, observed_job, state) {
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
