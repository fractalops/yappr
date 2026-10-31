//! Questions and final decisions in the writer's append-only feed.
use std::{collections::BTreeSet, fmt};

use anyhow::{bail, Result};
use iroh::EndpointId;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    disk,
    entry::{Draft, Entry},
    store::Store,
};

const FIELD: &str = "choices";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub title: String,
    pub body: String,
    pub source: String,
    pub options: Vec<Choice>,
    pub responder: EndpointId,
    pub expires_at: Option<Timestamp>,
    pub approval: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub question_id: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub question_id: String,
    pub submission_id: String,
    pub value: String,
    pub responder: EndpointId,
    pub time: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
    Question { question: Question },
    Decision { decision: Decision },
    Cancelled { question_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionState {
    pub question: Question,
    pub status: String,
    pub decision: Option<Decision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceError {
    pub code: String,
    pub message: String,
}
impl ChoiceError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl fmt::Display for ChoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ChoiceError {}

fn record(entry: &Entry) -> Option<Record> {
    entry
        .extra
        .get(FIELD)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn draft(record: Record, title: String, body: String, source: String, kind: &str) -> Result<Draft> {
    let mut extra = serde_json::Map::new();
    extra.insert(FIELD.into(), serde_json::to_value(record)?);
    Ok(Draft {
        source,
        kind: kind.into(),
        title: Some(title),
        body,
        extra,
    })
}

impl Question {
    pub fn validate(&self) -> Result<()> {
        if !valid_id(&self.id) {
            bail!("question ID is not valid");
        }
        if self.title.trim().is_empty() {
            bail!("give the question a title");
        }
        if !(2..=12).contains(&self.options.len()) {
            bail!("give between 2 and 12 options");
        }
        let mut values = BTreeSet::new();
        for option in &self.options {
            if option.value.is_empty()
                || option.value.len() > 64
                || !option
                    .value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                bail!("option values must use 1 to 64 letters, digits, underscores, or hyphens");
            }
            if option.label.trim().is_empty()
                || option.label.len() > 256
                || option.label.chars().any(char::is_control)
            {
                bail!("option labels must use 1 to 256 bytes on one line");
            }
            if !values.insert(&option.value) {
                bail!("option value {:?} occurs more than once", option.value);
            }
        }
        if self.approval && self.options != approval_options() {
            bail!("an approval must use approve and reject");
        }
        Draft {
            source: self.source.clone(),
            kind: "question".into(),
            title: Some(self.title.clone()),
            body: self.body.clone(),
            extra: Default::default(),
        }
        .validate()?;
        Ok(())
    }
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn approval_options() -> Vec<Choice> {
    vec![
        Choice {
            value: "approve".into(),
            label: "Approve".into(),
        },
        Choice {
            value: "reject".into(),
            label: "Reject".into(),
        },
    ]
}

pub fn create(store: &Store, question: Question) -> Result<Question> {
    question.validate()?;
    if question.expires_at.is_some_and(|t| t <= Timestamp::now()) {
        bail!("the deadline must be in the future");
    }
    let result = question.clone();
    store.transaction(|entries| {
        if entries.iter().any(
            |e| matches!(record(e), Some(Record::Question { question: q }) if q.id == question.id),
        ) {
            bail!("question ID already exists");
        }
        let entry = draft(
            Record::Question {
                question: question.clone(),
            },
            question.title.clone(),
            question.body.clone(),
            question.source.clone(),
            "question",
        )?;
        Ok((Some(entry), result))
    })
}

/// Question states, built one entry at a time.
#[derive(Debug, Default)]
pub struct Tracker {
    only: Option<String>,
    states: Vec<QuestionState>,
}

impl Tracker {
    /// Track one question and ignore the others.
    pub fn one(id: &str) -> Self {
        Self {
            only: Some(id.into()),
            states: Vec::new(),
        }
    }

    pub fn push(&mut self, entry: &Entry) {
        match record(entry) {
            Some(Record::Question { question })
                if self.only.as_ref().is_none_or(|id| *id == question.id) =>
            {
                self.states.push(QuestionState {
                    question,
                    status: "pending".into(),
                    decision: None,
                });
            }
            Some(Record::Decision { decision }) => {
                if let Some(state) = self.pending(&decision.question_id) {
                    state.status = "answered".into();
                    state.decision = Some(decision);
                }
            }
            Some(Record::Cancelled { question_id }) => {
                if let Some(state) = self.pending(&question_id) {
                    state.status = "cancelled".into();
                }
            }
            _ => {}
        }
    }

    fn pending(&mut self, id: &str) -> Option<&mut QuestionState> {
        self.states
            .iter_mut()
            .find(|s| s.question.id == id && s.status == "pending")
    }

    /// The states now. A pending question past its deadline is expired.
    pub fn states(&self) -> Vec<QuestionState> {
        let now = Timestamp::now();
        let mut states = self.states.clone();
        for state in &mut states {
            if state.status == "pending" && state.question.expires_at.is_some_and(|t| t <= now) {
                state.status = "expired".into();
            }
        }
        states
    }
}

pub fn list(entries: &[Entry]) -> Vec<QuestionState> {
    let mut tracker = Tracker::default();
    for entry in entries {
        tracker.push(entry);
    }
    tracker.states()
}

pub fn get(entries: &[Entry], id: &str) -> Result<QuestionState> {
    list(entries)
        .into_iter()
        .find(|s| s.question.id == id)
        .ok_or_else(|| ChoiceError::new("not_found", "the question does not exist").into())
}

pub fn decide(store: &Store, responder: EndpointId, submission: Submission) -> Result<Decision> {
    if !valid_id(&submission.id) || !valid_id(&submission.question_id) {
        return Err(
            ChoiceError::new("invalid", "the submission ID or question ID is not valid").into(),
        );
    }
    store.transaction(|entries| {
        let state = get(entries, &submission.question_id)?;
        if state.question.responder != responder || !store.allow_list().contains(&responder) {
            return Err(
                ChoiceError::new("denied", "this device cannot answer the question").into(),
            );
        }
        if let Some(decision) = state.decision {
            if decision.submission_id == submission.id && decision.value == submission.value {
                return Ok((None, decision));
            }
            return Err(
                ChoiceError::new("answered", "the question already has a final decision").into(),
            );
        }
        if state.status != "pending" {
            return Err(ChoiceError::new(
                &state.status,
                &format!("the question is {}", state.status),
            )
            .into());
        }
        let label = state
            .question
            .options
            .iter()
            .find(|o| o.value == submission.value)
            .map(|o| o.label.clone())
            .ok_or_else(|| ChoiceError::new("invalid", "the option does not exist"))?;
        let decision = Decision {
            question_id: submission.question_id,
            submission_id: submission.id,
            value: submission.value,
            responder,
            time: Timestamp::now(),
        };
        let entry = draft(
            Record::Decision {
                decision: decision.clone(),
            },
            format!("Choice: {label}"),
            String::new(),
            "yappr".into(),
            "decision",
        )?;
        Ok((Some(entry), decision))
    })
}

pub fn cancel(store: &Store, id: &str) -> Result<()> {
    store.transaction(|entries| {
        let state = get(entries, id)?;
        if state.status == "cancelled" {
            return Ok((None, ()));
        }
        if state.status != "pending" {
            bail!("the question is {}", state.status);
        }
        let entry = draft(
            Record::Cancelled {
                question_id: id.into(),
            },
            "Question cancelled".into(),
            String::new(),
            "yappr".into(),
            "cancelled",
        )?;
        Ok((Some(entry), ()))
    })
}

pub fn new_question(
    title: String,
    body: String,
    source: String,
    options: Vec<Choice>,
    responder: EndpointId,
    expires_at: Option<Timestamp>,
    approval: bool,
) -> Question {
    Question {
        id: disk::random_id(),
        title,
        body,
        source,
        options,
        responder,
        expires_at,
        approval,
    }
}

/// Parse `VALUE=Label`. Without a label, the value is also the label.
pub fn parse_option(text: &str) -> Result<Choice> {
    let (value, label) = text.split_once('=').unwrap_or((text, text));
    Ok(Choice {
        value: value.into(),
        label: label.into(),
    })
}
